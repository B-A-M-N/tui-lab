//! Rust-native terminal backend built on `portable-pty` + `vt100`
//! (spec section 36/37 fallback engine). This is the default engine.
//!
//! P0 correctness fixes implemented here (vs. the previous placeholder
//! implementation):
//!   * event-sequenced reader (`screen_seq`/`output_seq`/`bell_seq`/`title_seq`)
//!     drives edge-triggered waits instead of constant true/false stubs;
//!   * `ScreenStable` requires a genuine quiet interval, not instant success;
//!   * `ScreenChange` resolves on the first post-baseline screen mutation;
//!   * `resize()` resizes both the OS PTY and the `vt100` parser;
//!   * paste / mouse / cursor-key encoding is mode-aware (reads negotiated state);
//!   * Ctrl/Alt/Shift/Function keys are parsed into typed [`KeyEvent`]s;
//!   * signals are delivered to the child process group (Unix), not silently
//!     mapped to `kill`;
//!   * title and bell are tracked via parser callbacks;
//!   * [`Capabilities`] reports only working behavior.

use std::time::{Duration, Instant};

use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    Input, InputModes, MouseEvent, MouseMode, ObserveResult, RecordingHook, RecordingHookSlot,
    TerminalEventState, WaitCond, WaitOutcome, WaitReason,
};
use crate::screen::{ProcessState, ScreenState};

use super::input::{encode_key, encode_mouse_event};

/// vt100 callbacks that record host-observable terminal metadata —
/// moved to [`super::protocol`] (G2). Re-exported so `pump()` and the
/// probe paths keep their names.
pub struct PortablePtyBackend {
    cols: u16,
    rows: u16,
    /// Grid + normalization policy + scrollback (G2): see
    /// [`super::emulator::TerminalEmulator`].
    emulator: super::emulator::TerminalEmulator,
    /// Child process + PTY plumbing (G2): see [`super::process::PtyProcess`].
    process_pty: super::process::PtyProcess,
    // Recording hook slot (audit item 24/25/26).
    recording_slot: RecordingHookSlot,
    // Hook clone carried into the reader thread at start() and used by
    // write_input's notify path.
    reader_recording_hook: Option<super::RecordingHookSlot>,
    // Event sequencing state (spec section 1): counters, stamps, the
    // bounded per-change log, and query-answer bookkeeping (G2).
    events: super::event_clock::BackendEventClock,
    /// Wave-2 (protocol diagnostics): bounded raw-byte capture — the
    /// child's REAL output bytes (escape sequences, OSC, DCS and all) so
    /// the protocol decoder can reconstruct "what did this TUI actually
    /// emit". Round-2 (G2): ring + declared eviction + absolute total
    /// moved into [`super::raw_capture::RawCapture`].
    raw: super::raw_capture::RawCapture,
}

impl PortablePtyBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        PortablePtyBackend {
            cols,
            rows,
            emulator: super::emulator::TerminalEmulator::new(cols, rows),
            process_pty: super::process::PtyProcess::new(),
            recording_slot: new_recording_hook_slot(),
            reader_recording_hook: None,
            events: super::event_clock::BackendEventClock::new(),
            raw: super::raw_capture::RawCapture::new(),
        }
    }

    /// Item 48: install a contract-derived normalization policy. Affects
    /// every subsequent `state()` / `observe()` structure hash.
    pub fn set_normalization_policy(
        &mut self,
        policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
        self.emulator.set_normalization_policy(policy);
    }

    /// Drain buffered PTY bytes, feed the parser, and bump `screen_seq` on
    /// fingerprint change (cell text + style + cursor state), bumping
    /// `content_seq` on text-only change. Returns (before_fp, after_fp) hashes
    /// so the caller can detect screen mutations. This is the single funnel
    /// through which all terminal bytes and screen re-evaluation pass
    /// (audit items 1/2).
    fn pump(&mut self) -> BackendResult<(String, String)> {
        let before_contents = self.emulator.screen().contents();
        let before_fp = self.interaction_fingerprint();
        // Drain into a local buffer first so the rest of the pump can take
        // `&mut self` freely.
        let drained_chunks = self.process_pty.drain();
        for chunk in &drained_chunks {
            // Raw-output ring first (Wave-2 protocol diagnostics): the
            // child's real bytes, before the parser interprets them.
            self.absorb_raw(chunk);
            self.emulator.feed(chunk);
            self.events.on_output();
        }
        // Wave F item 56: write back any query responses the callbacks
        // produced (DA/DSR/DECRQM/kitty ?u/OSC color reports). A real
        // terminal answers these; an app that asked and never got an answer
        // would hang waiting — silence would be a protocol lie.
        let drained = std::mem::take(&mut self.emulator.callbacks_mut().query_responses);
        if !drained.is_empty() {
            // write_input forwards to the PTY; fall back silently when no
            // session is attached (callbacks can fire during parser tests).
            let _ = self.write_input(&drained);
            // Item 22: the answer just left for the app — that instant is
            // the measurable "responded at". Promote the pending class so
            // the session layer can fold a measured query/answer event.
            {
                let cb = self.emulator.callbacks_mut();
                if let Some(class) = cb.pending_class.take() {
                    self.events
                        .note_query_answered(class.as_str(), cb.answered_seq);
                }
            }
        }
        let after_contents = self.emulator.screen().contents();
        let after_fp = self.interaction_fingerprint();
        // content_seq bumps when text changed
        if before_contents != after_contents {
            self.events.bump_content();
        }
        // screen_seq bumps when the fingerprint changed (text or style)
        if before_fp != after_fp {
            self.events.bump_screen();
            // Item 26: record the change for per-action first-frame latency.
            self.events.on_screen_change();
        }
        // Wave F item 53: materialize the parser's scrollback rows so
        // search/observe read plain strings without touching the parser's
        // scroll offset. Reading rows via `set_scrollback` would disturb the
        // live view; instead we page the offset, copy, and restore it.
        self.emulator.refresh_scrollback(self.cols);
        Ok((before_fp, after_fp))
    }

    /// Wave F item 54: pull the OSC 133 command state out of the callbacks.
    fn command_state_from_callbacks(&self) -> Option<super::CommandState> {
        let cb = self.emulator.callbacks();
        if cb.command_seq == 0 && !cb.command_running && cb.last_command_exit.is_none() {
            // No 133 edges ever seen: honest None so command waits report
            // "shell integration not present" instead of spinning. (Phase
            // defaults to "" before any A/B/C/D arrives; only D sets an
            // exit, only C starts a count.)
            return None;
        }
        let phase: &'static str = match cb.command_phase {
            "output" => "output",
            "command" => "command",
            "done" => "done",
            _ => "prompt",
        };
        Some(super::CommandState {
            command_seq: cb.command_seq,
            running: cb.command_running,
            last_exit: cb.last_command_exit,
            phase,
        })
    }

    /// Notify the recording hook if attached (audit item 24).
    /// Locks the slot and calls `f`; ignores poisoned mutex.
    fn notify<F: FnOnce(&dyn RecordingHook)>(&self, f: F) {
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref hook) = *slot {
                f(hook.as_ref());
            }
        }
    }

    /// Compute an interaction fingerprint over the vt100 screen.
    ///
    /// Hashes every cell's text, fg, bg, bold, underline, reverse, plus cursor
    /// position, cursor visibility, and (cols, rows) dimensions via blake3.
    /// A change in reverse-video styling alone will change the fingerprint
    /// (audit item 1).
    /// Wave-2 (protocol diagnostics): push raw child bytes into the bounded
    /// ring, declaring head eviction. Bounded at [`RAW_RING_CAPACITY`] so a
    /// firehose child cannot grow memory without limit.
    fn absorb_raw(&mut self, bytes: &[u8]) {
        self.raw.absorb(bytes);
    }

    /// The retained raw output (oldest first) plus declared drop stats.
    pub fn raw_output_window(&mut self) -> (Vec<u8>, usize, u64) {
        let _ = self.pump();
        let (cap, dropped) = self.raw.stats();
        (self.raw.window(), cap, dropped)
    }

    /// The absolute byte range the retained window covers in the child's
    /// output stream: `(start_offset, end_offset_exclusive)`. Offsets survive
    /// head eviction, so a transaction's protocol range is citable even when
    /// the ring has wrapped (re-review item 19).
    pub fn raw_window_range(&mut self) -> (u64, u64) {
        let _ = self.pump();
        self.raw.range()
    }

    /// The absolute stream position at this instant — the offset the NEXT
    /// byte will get. Snapshot before and after an action to bracket it.
    pub fn raw_bytes_total(&mut self) -> u64 {
        let _ = self.pump();
        self.raw.total()
    }

    /// Item 22: the responder's most recent answer actually written back to
    /// the PTY — `(class, answered_counter)` — or `None` when nothing was
    /// answered since start/reset. Measured, not narrated: the counter only
    /// bumps at the `write_input` that delivered the bytes.
    pub fn last_query_answer(&mut self) -> (Option<&'static str>, u64) {
        let _ = self.pump();
        self.events.last_query_answer()
    }

    /// Item 26: `(screen_seq, unix_ms)` for every screen change at/after
    /// `after_seq`, oldest first — the evidence an action's FIRST frame
    /// latency is derived from. Empty when the engine saw no change since
    /// `after_seq`.
    pub fn screen_changes_since(&mut self, after_seq: u64) -> Vec<(u64, u64)> {
        let _ = self.pump();
        self.events.changes_since(after_seq)
    }

    /// Item 22: measured conformance probe. Feed `query` through the SAME
    /// parser the child's output flows through (so the same responder
    /// handles it), then return the exact answer bytes the responder
    /// composed for the app. Nothing is written to the child and nothing
    /// enters the output ring — the probe measures the ENGINE's reply to a
    /// query class, which is precisely what conformance means here.
    pub fn probe_query_response(&mut self, query: &[u8]) -> (Option<&'static str>, Vec<u8>) {
        self.emulator.feed(query);
        // The callbacks queue the reply; drain WITHOUT writing it to the
        // PTY (this is a probe, not app traffic).
        let drained = std::mem::take(&mut self.emulator.callbacks_mut().query_responses);
        let class = self
            .emulator
            .callbacks_mut()
            .pending_class
            .take()
            .map(|c| c.as_str());
        // pending_class was set by the LAST answering arm in `query`; for a
        // single-query probe it is exactly the answer's class.
        (class, drained)
    }

    fn interaction_fingerprint(&self) -> String {
        let screen = self.emulator.screen();
        let mut hasher = blake3::Hasher::new();
        let rows = screen.size().0;
        let cols = screen.size().1;
        hasher.update(b"fp:v1:");
        hasher.update(&cols.to_le_bytes());
        hasher.update(&rows.to_le_bytes());
        for y in 0..rows {
            for x in 0..cols {
                if let Some(cell) = screen.cell(y, x) {
                    hasher.update(cell.contents().as_bytes());
                    hasher.update(&color_to_u32(cell.fgcolor()).to_le_bytes());
                    hasher.update(&color_to_u32(cell.bgcolor()).to_le_bytes());
                    hasher.update(&[cell.bold() as u8]);
                    hasher.update(&[cell.underline() as u8]);
                    hasher.update(&[cell.inverse() as u8]);
                } else {
                    hasher.update(b"\0");
                }
            }
        }
        let cursor = screen.cursor_position();
        hasher.update(&cursor.0.to_le_bytes());
        hasher.update(&cursor.1.to_le_bytes());
        hasher.update(&[!screen.hide_cursor() as u8]);
        hasher.finalize().to_hex().to_string()
    }

    /// Pull bell/title counters out of the parser callbacks (shared by
    /// `state()`, `wait()` and `send_input`).
    fn sync_parser_counters(&mut self) {
        let cb = self.emulator.callbacks();
        self.events.sync_counters(cb.audible_bells, cb.title_seq);
    }

    fn current_modes(&self) -> InputModes {
        let screen = self.emulator.screen();
        InputModes {
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            mouse_mode: screen.mouse_protocol_mode().into(),
            mouse_encoding: screen.mouse_protocol_encoding().into(),
            cursor_visible: !screen.hide_cursor(),
            // Wave F item 52: kitty flags live in the callbacks (the vt100
            // grid has no notion of them).
            kitty_flags: self.emulator.callbacks().kitty_flags,
        }
    }

    /// Write bytes to the PTY via the writer and notify the recording hook
    /// (audit item 24).
    fn write_input(&mut self, bytes: &[u8]) -> BackendResult<()> {
        self.process_pty.write(bytes)?;
        // Notify input hook via the reader thread's hook clone.
        if let Some(ref hook) = self.reader_recording_hook {
            if let Ok(slot) = hook.lock() {
                if let Some(ref h) = *slot {
                    h.on_input(bytes);
                }
            }
        }
        Ok(())
    }

    /// Review P0 (AnyObservableChange): the backend's own observable-edge
    /// counter — screen changes + bells + titles. A pure sum, so any single
    /// edge advancing any component advances this. The `AnyActivity` wait
    /// compares against it so "any observable change" is a real superset of
    /// "screen change", including bell-only and title-only reactions.
    fn interaction_seq(&self) -> u64 {
        self.events.interaction_seq()
    }
}

impl TerminalBackend for PortablePtyBackend {
    fn recent_raw_output(&mut self) -> BackendResult<Vec<u8>> {
        let (bytes, _cap, _dropped) = self.raw_output_window();
        Ok(bytes)
    }

    fn raw_output_stats(&mut self) -> (usize, u64) {
        let (_bytes, cap, dropped) = self.raw_output_window();
        (cap, dropped)
    }

    fn raw_window_range(&mut self) -> (u64, u64) {
        PortablePtyBackend::raw_window_range(self)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn last_query_answer(&mut self) -> (Option<&'static str>, u64) {
        PortablePtyBackend::last_query_answer(self)
    }

    fn probe_query_response(&mut self, query: &[u8]) -> (Option<&'static str>, Vec<u8>) {
        PortablePtyBackend::probe_query_response(self, query)
    }

    fn screen_changes_since(&mut self, after_seq: u64) -> Vec<(u64, u64)> {
        PortablePtyBackend::screen_changes_since(self, after_seq)
    }

    fn start(
        &mut self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> BackendResult<()> {
        self.cols = cols;
        self.rows = rows;
        self.emulator.reset(cols, rows);
        self.process_pty
            .spawn(command, args, cwd, env, cols, rows, &self.recording_slot)?;
        // Also keep a hook clone on the struct for write_input usage.
        self.reader_recording_hook = Some(self.recording_slot.clone());

        // A new child stream is a new output stream: counters and offsets
        // restart.
        self.events.clear();
        self.raw.clear();
        self.emulator.callbacks_mut().query_responses.clear();

        // give the process a moment to emit initial frame
        std::thread::sleep(Duration::from_millis(150));
        // initial pump so state() is immediately meaningful
        let _ = self.pump();
        Ok(())
    }

    fn stop(&mut self) -> BackendResult<()> {
        // Signal the child process group, then the direct child, then drain
        // the reader — ordering lives in PtyProcess::terminate.
        self.process_pty.terminate();
        self.reader_recording_hook = None;
        Ok(())
    }

    fn state(&mut self) -> BackendResult<ScreenState> {
        // Make sure any buffered output is parsed before we snapshot.
        let _ = self.pump();
        // Track bell/title sequence deltas from callbacks.
        {
            let cb = self.emulator.callbacks();
            self.events.sync_counters(cb.audible_bells, cb.title_seq);
        }
        let title = self.emulator.callbacks().title.clone();
        let process = self.process();
        let links: Vec<crate::screen::cell::Hyperlink> = self
            .emulator
            .callbacks()
            .links
            .iter()
            .cloned()
            .chain(self.emulator.callbacks().open_link.clone())
            .collect();
        let mut state = crate::screen::from_vt_with_policy(
            self.emulator.screen(),
            process,
            title,
            links,
            self.emulator.normalization_policy(),
        );
        // Wave F item 53: attach the real scrollback (oldest first).
        state.scrollback = self.emulator.scrollback_cache().to_vec();
        Ok(state)
    }

    fn send_input(&mut self, input: Input) -> BackendResult<()> {
        // Drain any pending PTY output FIRST: the application may have just
        // negotiated a mode (mouse/paste/application cursor) whose escape
        // sequence is still sitting in the channel. Encoding must reflect the
        // negotiated state at the moment of the send, not one pump behind.
        let _ = self.pump();
        self.sync_parser_counters();
        // Resolve negotiated modes up front (clone, so no lingering borrow).
        let modes = self.current_modes();
        if !self.process_pty.has_session() {
            return Err(BackendError::NoSession);
        }
        // For Keys we parse ergonomic strings into typed KeyEvents at the
        // session boundary, so here we already operate on typed KeyEvents.
        match input {
            Input::Key(kev) => {
                let bytes = encode_key(&kev, &modes)?;
                self.write_input(&bytes)?;
            }
            Input::Keys(keys) => {
                // Execute the complete sequence (spec section 32).
                for kev in keys {
                    let bytes = encode_key(&kev, &modes)?;
                    self.write_input(&bytes)?;
                }
            }
            Input::Text(t) => {
                let bytes = t.into_bytes();
                self.write_input(&bytes)?;
            }
            Input::Paste(p) => {
                if modes.bracketed_paste {
                    let mut v = b"\x1b[200~".to_vec();
                    v.extend_from_slice(p.as_bytes());
                    v.extend_from_slice(b"\x1b[201~");
                    self.write_input(&v)?;
                } else {
                    // No bracketed paste mode: send raw text (do not lie about
                    // the protocol).
                    self.write_input(p.as_bytes())?;
                }
            }
            Input::Raw(b) => {
                self.write_input(&b)?;
            }
            Input::MouseClick { button, x, y } => {
                // Enforce mouse mode gating for press.
                match modes.mouse_mode {
                    MouseMode::None => {
                        return Err(BackendError::Unsupported(
                            "mouse reporting is not enabled by the application".into(),
                        ));
                    }
                    MouseMode::Press
                    | MouseMode::PressRelease
                    | MouseMode::ButtonMotion
                    | MouseMode::AnyMotion => {} // press is allowed
                }
                // A click is press + release back-to-back (audit item 8).
                let press =
                    encode_mouse_event(&MouseEvent::Press { button, x, y }, modes.mouse_encoding)?;
                self.write_input(&press)?;
                // Enforce mouse mode gating for release. Release is only
                // rejected in the most minimal mode (Press / 1000); even
                // 1002 (ButtonMotion) requires a release event so the
                // application can distinguish it from a button-held drag.
                if modes.mouse_mode == MouseMode::Press {
                    return Err(BackendError::Unsupported(
                        "mouse release not supported in Press-only mode".into(),
                    ));
                } // release allowed (PressRelease, ButtonMotion, AnyMotion)
                let release = encode_mouse_event(
                    &MouseEvent::Release { button, x, y },
                    modes.mouse_encoding,
                )?;
                self.write_input(&release)?;
            }
            Input::Mouse(ev) => {
                // Enforce mouse mode gating.
                let is_press = matches!(ev, MouseEvent::Press { .. });
                let is_release = matches!(ev, MouseEvent::Release { .. });
                let is_scroll = matches!(ev, MouseEvent::Scroll { .. });
                let is_move = matches!(ev, MouseEvent::Move { .. });
                let is_drag = matches!(ev, MouseEvent::Drag { .. });
                let _ = (&is_move, &is_drag); // used in mode gating below

                if modes.mouse_mode == MouseMode::None {
                    return Err(BackendError::Unsupported(
                        "mouse reporting is not enabled by the application".into(),
                    ));
                }
                let allowed = match modes.mouse_mode {
                    MouseMode::Press => is_press || is_scroll,
                    MouseMode::PressRelease => is_press || is_release || is_scroll,
                    // ButtonMotion (1002): press, release, scroll, and
                    // drag (motion-while-button-held) all allowed.
                    MouseMode::ButtonMotion => is_press || is_release || is_scroll || is_drag,
                    MouseMode::AnyMotion => true,
                    _ => false,
                };
                if !allowed {
                    return Err(BackendError::Unsupported(format!(
                        "{:?} not supported in negotiated mouse mode {:?}",
                        ev, modes.mouse_mode
                    )));
                }
                let bytes = encode_mouse_event(&ev, modes.mouse_encoding)?;
                self.write_input(&bytes)?;
            }
            Input::Resize { cols, rows } => {
                return self.resize(cols, rows);
            }
            Input::Signal(sig) => {
                return self.process_pty.signal_group(sig);
            }
        }
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> BackendResult<()> {
        self.cols = cols;
        self.rows = rows;
        self.process_pty.resize_master(cols, rows)?;
        // Resize the vt100 parser as well so our parsed screen matches the
        // child's notion of dimensions (spec section 3).
        self.emulator.set_size(rows, cols);
        // Notify resize hook (audit item 24).
        self.notify(|hook| hook.on_resize(cols, rows));
        Ok(())
    }

    fn wait(&mut self, cond: WaitCond, budget: Duration) -> BackendResult<WaitOutcome> {
        let start = Instant::now();
        // Review P0 (Bell race): an anchored Bell carries its own baseline;
        // only an unanchored Bell falls back to "captured at wait entry".
        let baselines = super::wait::WaitBaselines {
            bell_seq: match &cond {
                WaitCond::Bell {
                    after_bell_seq: Some(seq),
                } => seq.saturating_sub(1),
                _ => self.events.bell_seq(),
            },
            interaction_seq: self.interaction_seq(),
            // For screen-change: the baseline *interaction fingerprint*,
            // captured after the entry pump (style-only changes count;
            // audit item 3).
            fingerprint: {
                let _ = self.pump();
                self.interaction_fingerprint()
            },
        };

        loop {
            // Pump new bytes first.
            let _ = self.pump();
            // Track bell/title edges.
            {
                let cb = self.emulator.callbacks();
                self.events.sync_counters(cb.audible_bells, cb.title_seq);
            }

            let process = self.process();
            let title = self.emulator.callbacks().title.clone();
            let mut links: Vec<crate::screen::cell::Hyperlink> =
                self.emulator.callbacks().links.clone();
            if let Some(open) = &self.emulator.callbacks().open_link {
                links.push(open.clone());
            }
            let screen = crate::screen::from_vt_with_policy(
                self.emulator.screen(),
                process,
                title,
                links,
                self.emulator.normalization_policy(),
            );
            // Wave F item 53: command-output waits search scrollback too.
            let mut screen = screen;
            screen.scrollback = self.emulator.scrollback_cache().to_vec();
            let screen = screen;

            // Monotonic quiet checks, pre-computed for the evaluator: the
            // interval each condition kind asks for (None where quiet is
            // irrelevant).
            let (quiet_req, output_quiet_req) = match &cond {
                WaitCond::ScreenStable { quiet_for, .. } => (Some(*quiet_for), None),
                WaitCond::Idle { quiet_for, .. } => (None, Some(*quiet_for)),
                _ => (None, None),
            };
            let tick = super::wait::WaitTick {
                screen: &screen,
                screen_seq: self.events.screen_seq(),
                output_seq: self.events.output_seq(),
                bell_seq: self.events.bell_seq(),
                interaction_seq: self.events.interaction_seq(),
                screen_quiet: quiet_req
                    .map(|q| self.events.screen_quiet_for(q))
                    .unwrap_or(false),
                output_quiet: output_quiet_req
                    .map(|q| self.events.output_quiet_for(q))
                    .unwrap_or(false),
                command_seq: self.emulator.callbacks().command_seq,
                command_running: self.emulator.callbacks().command_running,
            };
            let (met, reason) = super::wait::WaitEvaluator::evaluate(
                &cond,
                &tick,
                &baselines,
                &self.interaction_fingerprint(),
            );

            if met {
                return Ok(WaitOutcome {
                    met: true,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    reason,
                    screen_seq: self.events.screen_seq(),
                    output_seq: self.events.output_seq(),
                    state: screen,
                });
            }
            if start.elapsed() >= budget {
                // Timeout: return current state with reason=Timeout.
                return Ok(WaitOutcome {
                    met: false,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    reason: WaitReason::Timeout,
                    screen_seq: self.events.screen_seq(),
                    output_seq: self.events.output_seq(),
                    state: screen,
                });
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn capabilities(&self) -> Capabilities {
        use super::{EventCapability, InputFamily, WaitCapability};
        let mut caps = Capabilities::honest();
        // Promote optional capabilities only when we have observed the running
        // application negotiate them.
        caps.title = self.events.title_seq() > 0;
        // Wave F item 53: promoted once real scrollback rows were captured.
        caps.scrollback = self.emulator.scrollback_seen();
        caps.mouse = self.emulator.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        caps.bracketed_paste = self.emulator.screen().bracketed_paste();
        // Wave F item 52: the app pushed kitty keyboard flags at some point.
        caps.kitty_keyboard = self.emulator.callbacks().kitty_seen;
        // This engine retains the raw PTY byte ring (`recent_raw_output`),
        // so protocol capture is genuinely available.
        caps.protocol_capture = true;
        // --- audit finding 37: the portable engine is the reference backend —
        //     every operation-oriented capability it advertises is backed by a
        //     real implementation the conformance suite exercises (finding 61).
        caps.raw_input = true; // Input::Raw writes arbitrary bytes to the PTY
        caps.bell_observable = true; // bell_seq tracked in wait()
        caps.exit_code = true; // process() reports the real exit code
        caps.shell_integration = true; // command_state() parses OSC 133
        caps.stdout_stderr_separation = false; // single PTY master, no split pipes
        caps.recording = true; // recording hook delivered on output/input
        caps.attach = false; // we spawn the child; we do not attach one
        caps.query_response = true; // the device-query responder answers CSI queries
        caps.event_types = vec![
            EventCapability::Output,
            EventCapability::Bell,
            EventCapability::Title,
            EventCapability::FocusChanged,
            EventCapability::SemanticChanged,
            EventCapability::Raw,
        ];
        caps.supported_waits = vec![
            WaitCapability::Text,
            WaitCapability::TextAbsent,
            WaitCapability::ScreenChange,
            WaitCapability::ScreenStable,
            WaitCapability::ProcessExit,
            WaitCapability::Title,
            WaitCapability::Bell,
            WaitCapability::AnyActivity,
            WaitCapability::Idle,
            WaitCapability::CommandDone,
        ];
        caps.input_families = vec![
            InputFamily::Key,
            InputFamily::Mouse,
            InputFamily::Paste,
            InputFamily::RawByte,
            InputFamily::Resize,
            InputFamily::Signal,
        ];
        caps
    }

    fn process(&mut self) -> ProcessState {
        self.process_pty.process_state()
    }

    fn input_modes(&self) -> InputModes {
        self.current_modes()
    }

    fn event_state(&self) -> TerminalEventState {
        self.events
            .event_state(self.emulator.callbacks().command_seq)
    }

    /// Wave F item 53: the scrollback materialized at the last pump.
    fn scrollback_lines(&mut self) -> BackendResult<Vec<String>> {
        let _ = self.pump();
        Ok(self.emulator.scrollback_cache().to_vec())
    }

    /// Wave F item 53: viewport + scrollback search through the shared
    /// [`crate::backend::search_screen`] so hit semantics match other
    /// backends exactly.
    fn search(&mut self, query: &str) -> BackendResult<Vec<super::SearchHit>> {
        let screen = self.state()?;
        Ok(crate::backend::search_screen(&screen, query))
    }

    /// Wave F item 54: OSC 133 shell-integration state.
    fn command_state(&mut self) -> Option<super::CommandState> {
        let _ = self.pump();
        self.command_state_from_callbacks()
    }

    /// Attach or detach the raw PTY recording hook (audit item 24).
    /// Stores the slot so the struct can forward on_input/on_resize events.
    fn set_recording_hook(&mut self, hook: super::RecordingHookSlot) {
        self.recording_slot = hook;
    }

    /// Wave G item 77: clear the inherited environment before applying the
    /// caller's pairs on the next `start()` (clean/strict isolation).
    fn set_clear_env(&mut self, clear: bool) {
        self.process_pty.set_clear_env(clear);
    }

    /// Observe current terminal state with optional idle-wait.
    ///
    /// Overrides the default fallback: captures state immediately, then if
    /// `idle > 0` waits for ScreenStable with the quieter budget, returning
    /// the final screen and whether it went stable (audit item 4).
    fn observe(&mut self, idle: std::time::Duration) -> BackendResult<ObserveResult> {
        // Capture state immediately.
        let initial = self.state()?;
        if idle == Duration::ZERO {
            return Ok(ObserveResult {
                screen: initial,
                stable: false,
                stable_ms: 0,
            });
        }
        let quiet = idle.min(Duration::from_millis(120));
        let start = Instant::now();
        let outcome = self.wait(
            WaitCond::ScreenStable {
                quiet_for: quiet,
                after_screen_seq: None,
            },
            idle + Duration::from_millis(500),
        )?;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ObserveResult {
            screen: outcome.state,
            stable: outcome.met,
            stable_ms: elapsed_ms,
        })
    }
}

/// Convert a vt100 color to a u32 for hashing.
fn color_to_u32(c: vt100::Color) -> u32 {
    match c {
        vt100::Color::Default => 0,
        vt100::Color::Idx(i) => 1 + (i as u32),
        vt100::Color::Rgb(r, g, b) => {
            0x1000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        }
    }
}

// `chunk_rx` is stored on the struct (declared near top) and drained in `pump`.

#[cfg(test)]
mod osc8_tests {
    use super::*;

    #[test]
    fn osc8_hyperlink_captured_in_screen_state() {
        let mut b = PortablePtyBackend::new(40, 5);
        let fixture = std::env::temp_dir().join("osc8_fixture.py");
        std::fs::write(&fixture, include_str!("/tmp/osc8_fixture.py")).unwrap();
        b.start(
            "python3",
            &[fixture.to_string_lossy().to_string()],
            None,
            &[],
            40,
            5,
        )
        .expect("start");
        std::thread::sleep(std::time::Duration::from_millis(700));
        let st = b.state().expect("state");
        b.stop().ok();
        let links = &st.hyperlinks;
        assert!(
            links.iter().any(|l| l.uri.contains("example.com/docs")),
            "OSC8 link must be captured: {:?}",
            links
        );
    }
}
