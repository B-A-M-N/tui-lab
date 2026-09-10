//! Observation and semantics: screen observation, native channel, fused frames, frame analysis.
//!
//! Impl-family extraction (Phase 2): the `Session` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. `Session` remains the single
//! per-session concurrency authority — no independent locking is
//! introduced. Signatures, visibility, and callers are unchanged.

use super::*;

impl Session {
    /// Observe: refresh the screen via the backend's event-aware settle, keep
    /// the previous frame for diffing, return the new frame, and emit the
    /// derived events (Wave B item 12): ScreenChanged / VisualChanged /
    /// CursorMoved / TitleChanged / ProcessExited / SemanticChanged-class
    /// transitions are derived by diffing against the previous frame.
    pub fn observe(&mut self, idle_ms: u64) -> anyhow::Result<ScreenState> {
        // Wave F items 58–63: drain any native semantic frames the app has
        // written since the last observation (bounded read, partial lines
        // stay pending). Native events (focus/activate/coverage) fold into
        // the session event queue HERE — mode-independent (re-review P1:
        // coverage accounting and native facts must not depend on which
        // observe mode a caller chose).
        self.native.poll();
        self.absorb_native_events();
        // Reader-thread byte/resize facts (item 15) fold in BEFORE the
        // backend read: they describe bytes the parser is about to consume,
        // so history stays ordered with the screen events derived from them.
        self.absorb_pending_ingest();
        let s = self
            .backend
            .observe(std::time::Duration::from_millis(idle_ms))?;
        // The parse inside observe() may have queued responder answers; the
        // pump that drains them runs within that same call, so fold them now
        // (item 22: measured query/response evidence lands in the stream).
        self.absorb_query_answers();
        // Advance the observation window ONCE: the returned old `last` is
        // both the diff base for event emission and the new `previous`
        // (the holder makes the take-then-install pairing atomic, so the
        // old double-take spurious-ProcessStarted bug cannot recur).
        match self.observation.advance(s.screen.clone()) {
            Some(p) => {
                self.emit_frame_events(&p, &s.screen);
            }
            None => {
                // First observation of a generation: process started.
                self.push_event(crate::events::TerminalEventKind::ProcessStarted);
            }
        }
        Ok(s.screen)
    }

    /// Wave F items 58–63: the session's native channel snapshot (or None
    /// when the app never wrote one).
    /// Wave 5 item 41: the source locus the app declared (over the native
    /// side channel) for the widget this target names. Targets are
    /// app-declared coverage strings — `#save`, `save.activate`,
    /// `widget:#save` — matched by their leading id segment against the
    /// latest native snapshot. Returns the node's `source` field, which
    /// the framework adapter attested. None when the target is not
    /// widget-shaped, the app sent no snapshot, or the node carries no
    /// locus.
    pub fn native_source_for_widget(
        &self,
        target: &str,
    ) -> Option<crate::semantic::source_ref::SourceRef> {
        // The id is the target's leading segment: strip `widget:` and take
        // up to the first `.`/`:` action separator.
        let id_part = target
            .strip_prefix("widget:")
            .unwrap_or(target)
            .split(['.', ':', '/'])
            .find(|s| !s.is_empty())?;
        let id = if id_part.starts_with('#') {
            id_part.to_string()
        } else {
            format!("#{id_part}")
        };
        let root = self.native_channel().latest.as_ref()?;
        root.flatten()
            .into_iter()
            .find(|(_, n)| n.id == id || n.id == id_part)
            .and_then(|(_, n)| n.source.clone())
    }

    pub fn native_channel(&self) -> &crate::semantic::native::NativeChannel {
        &self.native
    }

    /// Item 35: adapter AVAILABLE vs channel ACTIVE. These are different
    /// facts and conflating them lies in both directions:
    ///
    /// * `adapter_available` — the harness DID its part (the channel file
    ///   exists and `TUI_LAB_SEMANTIC` was injected into the launch env).
    ///   The app may still never read it.
    /// * `native_channel_active` — the app actually COOPERATED (≥1 valid
    ///   frame accepted). An app that started but never wrote a frame
    ///   keeps this false forever, by evidence.
    /// * `frames_accepted` / `frames_invalid` — the health split: a
    ///   channel receiving ONLY invalid frames is active-but-unhealthy,
    ///   which is exactly the bug shape an agent needs to see.
    pub fn adapter_status(&self) -> crate::semantic::adapter_status::AdapterStatus {
        crate::semantic::adapter_status::AdapterStatus {
            adapter_available: self.native.path.is_some(),
            native_channel_active: self.native.frames_accepted > 0,
            frames_received: self.native.frames_accepted,
            frames_invalid: self.native.frames_invalid,
            healthy: self.native.frames_accepted > 0 && self.native.frames_invalid == 0,
        }
    }

    /// Item 29: native coverage targets the app has reported so far this
    /// session (the `coverage` event stream). Empty for non-cooperative
    /// apps — the caller's novelty ledger just loses that dimension.
    pub fn native_coverage_targets(&self) -> Vec<String> {
        self.native
            .events
            .iter()
            .filter(|e| e.event == "coverage")
            .map(|e| e.target.clone())
            .collect()
    }

    /// Drain the native channel NOW (bounded read; partial lines stay
    /// pending) and fold fresh events into the queue. Necessary before any
    /// fused read that must not be stale: `native.poll()` otherwise runs
    /// only inside `observe()`, so a cooperative app's post-action declare
    /// can sit unread in the channel file when a transaction's after-side
    /// focus is computed from settle-wait state alone.
    pub fn poll_native(&mut self) {
        self.native.poll();
        self.absorb_native_events();
    }

    /// Wave-2 (protocol diagnostics): the backend's retained raw output
    /// window — `(bytes, ring_capacity, bytes_dropped)` — for the protocol
    /// decoder. `(0, 0)` capacity means the engine retains nothing; a
    /// failed READ is an `Err`, not silent empty bytes (audit P1-44: the
    /// old `unwrap_or_default()` made "the ring errored" look like "the
    /// engine captured nothing", and a protocol trace was decoded from
    /// zero bytes and reported as truth).
    pub fn raw_output_window(&mut self) -> crate::backend::BackendResult<(Vec<u8>, usize, u64)> {
        let bytes = self.backend.recent_raw_output()?;
        let (cap, dropped) = self.backend.raw_output_stats();
        Ok((bytes, cap, dropped))
    }

    /// The absolute byte offset of the child's output stream at this instant
    /// (re-review item 19) — snapshot before and after an action to bracket
    /// its exact protocol bytes. `(0, 0)` from engines without raw capture.
    pub fn raw_window_range(&mut self) -> (u64, u64) {
        self.backend.raw_window_range()
    }

    /// Item 22: the responder's most recent delivered answer —
    /// `(class, answered_counter)`; `(None, 0)` from engines without a
    /// responder. Measured at the `write_input` that delivered the bytes.
    pub fn last_query_answer(&mut self) -> (Option<&'static str>, u64) {
        // G3: typed optional trait method — engines without a responder
        // return the honest `(None, 0)` default.
        self.backend.last_query_answer()
    }

    /// Item 22: measured conformance probe — feed `query` through the same
    /// parser the child's output uses and return the responder's exact
    /// answer bytes `(class, answer)`. Nothing reaches the child; nothing
    /// enters the output ring. `(None, empty)` from engines without a
    /// responder.
    pub fn probe_query_response(&mut self, query: &[u8]) -> (Option<&'static str>, Vec<u8>) {
        self.backend.probe_query_response(query)
    }

    /// Item 26: `(screen_seq, unix_ms)` for every screen change at/after
    /// `after_seq`, oldest first — the measured evidence for an action's
    /// first-frame latency. Empty when no change was observed.
    pub fn screen_changes_since(&mut self, after_seq: u64) -> Vec<(u64, u64)> {
        self.backend.screen_changes_since(after_seq)
    }

    /// Wave-2 (streams): the pipe engine's genuine stdout/stderr line
    /// separation. Returns `(stdout, stderr)`; `(empty, empty)` on engines
    /// that interleave by construction.
    pub fn pipe_streams(&mut self) -> (Vec<String>, Vec<String>) {
        // G3: typed optional trait method — the pipe engine overrides it;
        // interleaving engines return the honest `(empty, empty)` default.
        self.backend.separated_streams()
    }

    /// Overlay the latest native snapshot onto an inferred semantic tree,
    /// returning the merge report. Native claims win over inference with
    /// `source: "native"` / confidence 1.0; unmatched native ids are
    /// reported (never dropped).
    pub fn overlay_native(
        &self,
        tree: &mut crate::semantic::node::SemanticTree,
    ) -> crate::semantic::native::NativeOverlayReport {
        self.native.overlay(tree)
    }

    /// The most recent observation.
    pub fn last(&self) -> Option<&ScreenState> {
        self.observation.last()
    }

    /// The observation before `last`, if two observations have been made since
    /// the last (re)start. Used for real previous→current diffs (audit item 12).
    pub fn previous(&self) -> Option<&ScreenState> {
        self.observation.previous()
    }

    /// Test-only observation seeding: install a screen as the session's
    /// last settled observation without a backend round-trip, so unit
    /// tests can exercise consumers of `last()` (frame provenance,
    /// diffing) against a known screen. `#[cfg(test)]` — production code
    /// must go through `observe()`.
    #[cfg(test)]
    pub(crate) fn seed_last_observation(&mut self, screen: ScreenState) {
        let _ = self.observation.advance(screen);
    }

    /// Semantic analysis of the last settled frame, served from the per-session
    /// [`crate::semantic::SemanticCache`]. Returns `None` when no observation
    /// has happened yet. The cache is keyed on `ScreenState::structure_hash`, so
    /// an unchanged frame returns instantly instead of re-running all seven
    /// detectors (Wave G review P1 15/16).
    pub fn analyze_frame(&self) -> Option<crate::semantic::CacheResult> {
        let screen = self.last()?;
        Some(self.semantic.cache().borrow_mut().analyze(screen))
    }

    /// Structural-cache health (re-review item 41): hit counts surfaced on
    /// summary so the cache's work is evidence, not folklore.
    pub fn semantic_cache_hits(&self) -> u64 {
        self.semantic.cache().borrow().hits()
    }

    pub fn semantic_cache_misses(&self) -> u64 {
        self.semantic.cache().borrow().misses()
    }

    pub fn semantic_cache_hit_rate(&self) -> f32 {
        self.semantic.cache().borrow().hit_rate()
    }

    /// The fused semantic truth for the last settled frame (re-review
    /// Wave-4): one cached detection pass producing both shapes, then the
    /// native overlay applied fresh. Every semantic-bearing observe mode
    /// (summary / semantic / tree / nodes) and the `tui://` semantic
    /// resource routes through here, so they cannot disagree with each
    /// other. Returns `None` when no observation has happened yet.
    ///
    /// Item 52 (reactive FrameCommit): the fused result is memoized on
    /// the frame's full identity (structure + interaction + native
    /// position). Unchanged identity → the memo serves all three shapes
    /// without re-running the interaction pass or the overlay; any
    /// invalidating input recomputes once. `fuse_screen` bypasses the
    /// memo (arbitrary frames stay pure), and `poll_native`/observe
    /// invalidate through the key change.
    pub fn fused_frame(
        &self,
    ) -> Option<(
        crate::semantic::SemanticScreen,
        crate::semantic::node::SemanticTree,
        crate::semantic::native::NativeOverlayReport,
    )> {
        let screen = self.last()?;
        let key = crate::session::semantic_state::fused_key(
            screen,
            self.event_state.native_absorbed_seq(),
        );
        if let Some(hit) = self.semantic.memo_hit(&key) {
            return Some(hit);
        }
        let (sem, tree, report) = crate::semantic::fuse(
            screen,
            &mut self.semantic.cache().borrow_mut(),
            &self.native,
        );
        self.semantic
            .store_memo(key, sem.clone(), tree.clone(), report.clone());
        Some((sem, tree, report))
    }

    /// How many fused reads the memo served without recompute (item 52
    /// evidence, surfaced next to the structural cache stats).
    pub fn fused_memo_hits(&self) -> u64 {
        self.semantic.fused_memo_hits()
    }

    /// Drop the fused memo. Callers that change fused inputs OUTSIDE the
    /// frame identity (currently none — the key covers structure,
    /// interaction, and native position) would use this; kept as the
    /// explicit invalidation hook so the reactive contract has a manual
    /// escape hatch.
    pub fn invalidate_fused(&self) {
        self.semantic.invalidate_fused();
    }

    /// Fused analysis of an ARBITRARY frame owned by a transaction (re-view
    /// P0: transaction focus/semantic evidence must see native facts, not a
    /// bare re-inference — the old `frame_focus` called `semantic::analyze`
    /// directly, so a cooperative app's self-reported focus was invisible to
    /// exactly the evidence path that feeds the focus graph). Same cache +
    /// native overlay as `fused_frame`, applied to the given frame.
    pub fn fuse_screen(
        &self,
        screen: &crate::screen::ScreenState,
    ) -> crate::semantic::SemanticScreen {
        let (sem, _tree, _report) = crate::semantic::fuse(
            screen,
            &mut self.semantic.cache().borrow_mut(),
            &self.native,
        );
        sem
    }

    /// Fused analysis of an ARBITRARY frame, returning the FULL result
    /// (semantic screen + tree + identity inputs, not just the screen). Same
    /// cache + native overlay as `fuse_screen`/`fused_frame`, so a frame's
    /// identity computed here matches what `tui_observe semantic` reports.
    /// Used by the execution path to stamp `CanonicalFrame.semantic_identity`
    /// at capture time (review P0.5: evidence serializes truth, never
    /// recomputes it).
    pub fn fuse_frame_full(
        &self,
        screen: &crate::screen::ScreenState,
    ) -> Option<(
        crate::semantic::SemanticScreen,
        crate::semantic::node::SemanticTree,
    )> {
        let (sem, tree, _report) = crate::semantic::fuse(
            screen,
            &mut self.semantic.cache().borrow_mut(),
            &self.native,
        );
        Some((sem, tree))
    }

    /// Observe, then return the fused analysis of the fresh frame (re-review
    /// Wave-2 item 16: THE way subsystems read semantics — no direct
    /// `semantic::analyze` above the frame pipeline). Structural detection
    /// is cached; the interaction pass and native overlay run fresh.
    pub fn observe_fused(
        &mut self,
        idle_ms: u64,
    ) -> anyhow::Result<(
        crate::screen::ScreenState,
        crate::semantic::SemanticScreen,
        crate::semantic::node::SemanticTree,
        crate::semantic::native::NativeOverlayReport,
    )> {
        let screen = self.observe(idle_ms)?;
        let (sem, tree, report) = self
            .fused_frame()
            .expect("observe() seeded `last`, so fused_frame() has a frame");
        Ok((screen, sem, tree, report))
    }

    /// Current fused snapshot WITHOUT advancing the observation cursor.
    ///
    /// Pumps pending backend/native facts and fuses the current grid, but
    /// `last`/`previous` remain the public settled observation window. The
    /// next explicit `observe()` still diffs against the user's own prior
    /// observation, so guards/resources/preflights cannot rewrite that
    /// history.
    pub fn snapshot_fresh(&mut self) -> anyhow::Result<crate::session::state::FrameAnalysis> {
        self.native.poll();
        self.absorb_native_events();
        self.absorb_pending_ingest();
        let screen = self.backend.state()?;
        self.absorb_query_answers();
        Ok(self.analyze_screen(screen))
    }

    /// Backward-compatible name for internal pre-dispatch snapshots.
    pub fn peek_fresh(&mut self) -> anyhow::Result<crate::session::state::FrameAnalysis> {
        self.snapshot_fresh()
    }

    /// Native semantic channel revision, suitable for exact stale-state
    /// guards. Frames accepted by the channel advance this; an absent
    /// channel is a stable `None`.
    pub fn native_revision(&self) -> Option<u64> {
        self.native.revision()
    }

    /// THE authoritative per-frame analysis (re-review P0.4): one struct
    /// carrying the frame plus its fused semantic screen, semantic tree,
    /// native overlay report, and the fused semantic identity. Every
    /// proof-producing subsystem — contract conformance, scenario replay,
    /// audit residue detection, exploration state identity, assertions —
    /// consumes THIS, so a cooperative app's native semantics are visible
    /// everywhere and no two verdict paths can disagree.
    ///
    /// `analyze_last` serves the cached live frame; `analyze_screen`
    /// analyzes an arbitrary frame a transaction owns (before/after), with
    /// the native overlay positioned at the CURRENT channel state — the
    /// honest option for historical frames, since overlay facts older than
    /// the frame cannot be reconstructed after the channel advanced.
    pub fn analyze_screen(
        &self,
        screen: crate::screen::ScreenState,
    ) -> crate::session::state::FrameAnalysis {
        let (sem, tree, native) = crate::semantic::fuse(
            &screen,
            &mut self.semantic.cache().borrow_mut(),
            &self.native,
        );
        let semantic_identity = crate::semantic::semantic_identity_fused(&sem, &tree);
        crate::session::state::FrameAnalysis {
            frame: screen,
            semantic: sem,
            tree,
            native,
            semantic_identity,
        }
    }

    /// Fused [`FrameAnalysis`] of the session's most recent frame.
    pub fn analyze_last(&self) -> Option<crate::session::state::FrameAnalysis> {
        self.last().map(|s| self.analyze_screen(s.clone()))
    }
}
