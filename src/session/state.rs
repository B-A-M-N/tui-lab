//! A single TUI session: owns a backend and the most recent screen snapshot.
//!
//! A session is now durable (spec section 11/13): it stores the exact
//! [`LaunchSpec`] used to start the target, so a `restart()` relaunches the
//! *same* program with the *same* args/cwd/env/dimensions — and keeps the same
//! session id while bumping `generation`, because a restart is the next
//! generation of the same logical session, not a replacement.

use crate::backend::{
    Capabilities, PortablePtyBackend, PtyLineBackend, RecordingHook, RecordingHookSlot,
    TerminalBackend, TerminalEventState, WaitCond, WaitOutcome,
};
use crate::recording::AsciicastRecorder;
use crate::screen::{ProcessState, ScreenState};

// Impl-family extractions (Phase 2): each hosts the method bodies for one
// subsystem while the struct + fields stay here. `Session` remains the single
// per-session concurrency authority — no independent locking.
#[path = "control_impl.rs"]
mod control_impl;
#[path = "events_impl.rs"]
mod events_impl;
#[path = "process_impl.rs"]
mod process_impl;
#[path = "recording_impl.rs"]
mod recording_impl;
#[path = "semantic_impl.rs"]
mod semantic_impl;

/// Exact engine identity (re-review P0: no boolean classification). Backend
/// selection compares THIS, not string classes — the old `wants_line !=
/// current_is_line` test could not distinguish pipe from portable PTY, so a
/// fresh session (which always starts portable) never swapped to a requested
/// pipe engine: the spec said pipe, the process got a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackendKind {
    /// Portable PTY + vt100 grid (fullscreen TUIs). `auto` resolves here.
    #[serde(rename = "portable-pty+vt100")]
    PortableVt,
    /// Real PTY driven line-by-line (CLIs, REPLs, hybrid shells).
    #[serde(rename = "line-cli+lines")]
    PtyLine,
    /// True pipes: no PTY, isatty() false, separated stdout/stderr.
    #[serde(rename = "pipe")]
    Pipe,
    /// Attach to an ALREADY-RUNNING TUI inside a tmux pane (re-review
    /// item 18): render via `capture-pane`, input via `send-keys`, resize
    /// via `resize-pane`. The "command" in the LaunchSpec is the tmux
    /// target string `session:window.pane`.
    #[serde(rename = "tmux-attach")]
    TmuxAttach,
}

impl BackendKind {
    /// Parse the agent-facing LaunchSpec/MCP name. Unknown names are an
    /// error, never a silent fallback. Accepts BOTH the agent-facing aliases
    /// (`auto`, `portable_vt100`, `cli`) and this enum's own `display()`
    /// / serde names — a round-trip through the launch layer (typed kind →
    /// display name → spec → parse) must never fail to re-parse itself.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        Ok(match name {
            "auto" | "portable_vt100" | "portable-pty+vt100" => BackendKind::PortableVt,
            "cli" | "line_cli" | "line-cli+lines" => BackendKind::PtyLine,
            "pipe" => BackendKind::Pipe,
            "tmux" | "tmux_attach" | "tmux-attach" => BackendKind::TmuxAttach,
            other => {
                return Err(anyhow::anyhow!(
                    "unknown backend '{other}' (supported: auto, portable_vt100, cli, line_cli, pipe, tmux)"
                ))
            }
        })
    }

    /// The engine's honest display name for session status.
    pub fn display(&self) -> &'static str {
        match self {
            BackendKind::PortableVt => "portable-pty+vt100",
            BackendKind::PtyLine => "line-cli+lines",
            BackendKind::Pipe => "pipe",
            BackendKind::TmuxAttach => "tmux-attach",
        }
    }

    /// Build the engine. The ONE construction site (make_backend folded in).
    fn build(self, cols: u16, rows: u16) -> anyhow::Result<Box<dyn TerminalBackend>> {
        Ok(match self {
            BackendKind::PortableVt => Box::new(PortablePtyBackend::new(cols, rows)),
            BackendKind::PtyLine => Box::new(PtyLineBackend::new(cols, rows)),
            BackendKind::Pipe => Box::new(crate::backend::pipe::PipeBackend::new(cols, rows)),
            BackendKind::TmuxAttach => {
                return Err(anyhow::anyhow!(
                    "the tmux backend is built by attach(); select backend=tmux with tui_session action=attach"
                ))
            }
        })
    }
}

/// The exact launch configuration for a target program (spec section 11).
///
/// Stored permanently on the session. Every restart/reproduction uses the same
/// spec unless explicitly overridden.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LaunchSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub backend: String,
    pub isolation: String,
}

impl LaunchSpec {
    pub fn new(command: &str, cols: u16, rows: u16) -> Self {
        LaunchSpec {
            command: command.to_string(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols,
            rows,
            backend: "auto".to_string(),
            isolation: "local".to_string(),
        }
    }
}

/// The single-authority analysis of one terminal frame (re-review P0.4):
/// the raw frame plus its FUSED semantic screen, semantic tree, native
/// overlay report, and fused semantic identity. Produced only by a
/// [`Session`] (`analyze_screen` / `analyze_last` / `observe_fused`
/// territory), so every consumer sees the same native-participating truth
/// and the old bare `semantic::analyze` re-inference paths — contract
/// conformance, audit residue, exploration identity, scenario oracles —
/// cannot disagree with what `tui_observe semantic` reports.
#[derive(Debug, Clone)]
pub struct FrameAnalysis {
    /// The analyzed frame.
    pub frame: crate::screen::ScreenState,
    /// Fused semantic screen (inferred detection + native overlay).
    pub semantic: crate::semantic::SemanticScreen,
    /// Fused semantic tree.
    pub tree: crate::semantic::node::SemanticTree,
    /// What the native overlay joined (matched ids, native-only insertions,
    /// applied focus).
    pub native: crate::semantic::native::NativeOverlayReport,
    /// Fused semantic identity — structure + interaction + native truth.
    pub semantic_identity: String,
}

impl FrameAnalysis {
    /// Convenience: the analyzed frame.
    pub fn screen(&self) -> &crate::screen::ScreenState {
        &self.frame
    }
}

/// Rows whose cell text differs between two frames (Wave B item 12:
/// `ScreenChanged.dirty_rows` derived at the only place with both frames).
/// The semantic half of the frame diff (audit finding 28): compare the two
/// frames' analyzed semantics and push FocusChanged / SemanticChanged when
/// the advertised vocabulary's premises actually hold. A free function so
/// the decision is testable without a PTY session.
fn emit_semantic_frame_events(
    events: &mut crate::events::TerminalEventQueue,
    session: &str,
    generation: u32,
    prev: &ScreenState,
    next: &ScreenState,
    cache: &mut crate::semantic::SemanticCache,
) {
    let prev_sem = cache.analyze(prev).sem;
    let next_sem = cache.analyze(next).sem;
    if prev_sem.focus.control_id != next_sem.focus.control_id
        || prev_sem.focus.control != next_sem.focus.control
    {
        events.push(
            session,
            generation,
            crate::events::TerminalEventKind::FocusChanged {
                from: prev_sem.focus.control_id.or(prev_sem.focus.control),
                to: next_sem.focus.control_id.or(next_sem.focus.control),
            },
        );
    }
    if prev.semantic_identity() != next.semantic_identity() {
        events.push(
            session,
            generation,
            crate::events::TerminalEventKind::SemanticChanged,
        );
    }
}

/// Compares viewport text per row — cheap, and matches what an incremental
/// reader would fetch.
fn dirty_rows(prev: &ScreenState, next: &ScreenState) -> Vec<u16> {
    let mut out = Vec::new();
    let rows = prev.viewport_text.len().max(next.viewport_text.len());
    for y in 0..rows {
        let a = prev.viewport_text.get(y);
        let b = next.viewport_text.get(y);
        if a != b {
            out.push(y as u16);
        }
    }
    out
}

pub struct Session {
    pub id: String,
    pub command: String,
    pub backend_kind: BackendKind,
    pub generation: u32,
    launch: Option<LaunchSpec>,
    backend: Box<dyn TerminalBackend>,
    /// Capabilities snapshot captured at start. Kept for historical comparison
    /// only — live capability queries go through [`Session::capabilities`],
    /// which re-queries the backend so post-start negotiation (mouse, paste,
    /// title) is visible (audit item 11).
    caps_at_start: Capabilities,
    /// The settled-observation window `(previous, last)` (G4): see
    /// [`super::observation::ObservationState`].
    observation: super::observation::ObservationState,
    /// Recording sink + input flag (G4): see
    /// [`super::recording_state::RecordingState`].
    recording: super::recording_state::RecordingState,
    /// Raw PTY byte-stream hook slot; attached to the backend at start.
    recording_slot: RecordingHookSlot,
    /// Byte/resize facts observed by the reader thread, pending absorption
    /// into the event queue (re-review item 15: output events come from the
    /// reader thread via the session hook; `observe()` folds them in).
    pending_ingest: std::sync::Arc<std::sync::Mutex<Vec<Ingest>>>,
    /// The recorder slot INSIDE the session hook, shared so recording can be
    /// toggled without replacing the hook (event ingestion never pauses).
    hook_recorder: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>>>,
    >,
    /// Event stream + absorption bookkeeping (G4): the queue, per-consumer
    /// cursors, anchor counter, and the query/native watermarks — see
    /// [`super::event_state::SessionEventState`].
    event_state: super::event_state::SessionEventState,
    /// Frame-analysis state (G4): the structural semantic cache, the
    /// reactive fused memo (item 52), and its hit counter — see
    /// [`super::semantic_state::SemanticState`].
    semantic: super::semantic_state::SemanticState,
    /// Wave F items 58–63: the NativeSemanticProtocol side channel. The
    /// session creates it at start and injects `TUI_LAB_SEMANTIC` into the
    /// child's env; cooperative apps write their real semantic tree there
    /// and observation merges it over inference.
    native: crate::semantic::native::NativeChannel,
    /// Wave G item 76: the human control lease, when one is held.
    lease: crate::session::lease::LeaseState,
    /// Wave G item 77: what the current generation's launch actually got
    /// (profile, network isolation, env policy) — evidence, not a promise.
    isolation_evidence: Option<crate::session::isolation::IsolationEvidence>,
}

/// Bridge that feeds raw PTY bytes into the session's [`AsciicastRecorder`]
/// AND byte/resize facts into the session event queue (re-review item 15:
/// "bytes arrived" is the most fundamental thing that happens on a
/// terminal — history without it cannot answer "what happened?"). Lives
/// behind `Arc<dyn RecordingHook>` on the backend's reader thread, attached
/// for the WHOLE session lifetime (not only while recording). The event
/// side carries byte COUNTS, never the bytes themselves — payloads can
/// contain secrets; counts are safe and useful for replay forensics.
struct SessionHook {
    /// Byte-chunk sizes and resizes, drained into the event queue by the
    /// next `observe()`.
    ingest: std::sync::Arc<std::sync::Mutex<Vec<Ingest>>>,
    /// The active recorder, when recording is on — shared with the session
    /// so `enable_recording`/`stop_recording` swap it WITHOUT touching the
    /// hook slot (event ingestion is never interrupted).
    recorder: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>>>,
    >,
}

/// What the reader thread observed, pending absorption into the event queue.
enum Ingest {
    Output(usize),
    Resize(u16, u16),
}

impl RecordingHook for SessionHook {
    fn on_output(&self, bytes: &[u8]) {
        if let Ok(mut q) = self.ingest.lock() {
            q.push(Ingest::Output(bytes.len()));
        }
        if let Ok(rec) = self.recorder.lock() {
            if let Some(rec) = rec.as_ref() {
                if let Ok(mut rec) = rec.lock() {
                    rec.record_output(bytes);
                }
            }
        }
    }
    fn on_input(&self, bytes: &[u8]) {
        if let Ok(rec) = self.recorder.lock() {
            if let Some(rec) = rec.as_ref() {
                if let Ok(mut rec) = rec.lock() {
                    rec.record_input(bytes);
                }
            }
        }
    }
    fn on_resize(&self, cols: u16, rows: u16) {
        if let Ok(mut q) = self.ingest.lock() {
            q.push(Ingest::Resize(cols, rows));
        }
        if let Ok(rec) = self.recorder.lock() {
            if let Some(rec) = rec.as_ref() {
                if let Ok(mut rec) = rec.lock() {
                    rec.record_resize(cols, rows);
                }
            }
        }
    }
}

impl Session {
    /// Wave F items 50–51: backend selection. `auto` / `portable_vt100` map
    /// to the PTY engine; `cli` / `line_cli` map to the line CLI engine.
    /// Unknown names are an error — never a silent fallback.
    pub fn new(id: String, command: String) -> Self {
        let mut s = Session {
            id,
            command,
            backend_kind: BackendKind::PortableVt,
            generation: 1,
            launch: None,
            backend: Box::new(PortablePtyBackend::new(80, 24)),
            caps_at_start: Capabilities::default(),
            observation: super::observation::ObservationState::new(),
            recording: super::recording_state::RecordingState::new(),
            recording_slot: crate::backend::new_recording_hook_slot(),
            pending_ingest: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            hook_recorder: std::sync::Arc::new(std::sync::Mutex::new(None)),
            event_state: super::event_state::SessionEventState::new(),
            semantic: super::semantic_state::SemanticState::new(),
            native: crate::semantic::native::NativeChannel::default(),
            lease: crate::session::lease::LeaseState::default(),
            isolation_evidence: None,
        };
        s.attach_session_hook();
        s
    }

    /// Attach the session's permanent reader-thread hook: byte/resize facts
    /// flow into the event queue for the whole session lifetime (re-review
    /// item 15), and the recorder — when enabled — receives the same bytes.
    /// One hook slot, one hook, two consumers; `enable_recording` swaps the
    /// recorder INSIDE the hook instead of replacing it, so event ingestion
    /// is never interrupted by recording toggling.
    fn attach_session_hook(&mut self) {
        let hook: std::sync::Arc<dyn RecordingHook> = std::sync::Arc::new(SessionHook {
            ingest: self.pending_ingest.clone(),
            recorder: self.hook_recorder.clone(),
        });
        *self.recording_slot.lock().expect("recording slot") = Some(hook);
        self.backend.set_recording_hook(self.recording_slot.clone());
    }

    /// Drain the reader thread's pending byte/resize facts into the event
    /// queue. Called from `observe()` so absorption is synchronous with the
    /// parse that consumed the same bytes.
    fn absorb_pending_ingest(&mut self) {
        let pending: Vec<Ingest> = match self.pending_ingest.lock() {
            Ok(mut q) => std::mem::take(q.as_mut()),
            Err(_) => Vec::new(),
        };
        for fact in pending {
            match fact {
                Ingest::Output(n) => {
                    self.push_event(crate::events::TerminalEventKind::Output { byte_len: n })
                }
                Ingest::Resize(c, r) => {
                    self.push_event(crate::events::TerminalEventKind::Resize { cols: c, rows: r })
                }
            }
        }
    }

    /// Item 22: fold the backend responder's newly-delivered answers into
    /// the event queue as measured `QueryAnswered` events. Deduplicated by
    /// the monotonic answered counter, so repeated calls between pumps are
    /// idempotent. `None` from engines without a responder changes nothing.
    fn absorb_query_answers(&mut self) {
        // G3: typed optional trait method; engines without a responder
        // honestly never report answers.
        let (class, answered_seq) = self.backend.last_query_answer();
        if let Some(class) = class {
            if answered_seq > self.event_state.last_query_answered_seq() {
                self.event_state.set_query_answered_seq(answered_seq);
                self.push_event(crate::events::TerminalEventKind::QueryAnswered {
                    class: class.to_string(),
                });
            }
        }
    }

    /// Point the session hook at the given recorder (or none). The hook slot
    /// itself is untouched — the backend keeps delivering bytes to the event
    /// queue and to whichever recorder is currently installed.
    fn set_hook_recorder(
        &mut self,
        rec: Option<std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>>,
    ) {
        if let Ok(mut inner) = self.hook_recorder.lock() {
            *inner = rec;
        }
    }

    /// Diff two frames and push the events the transition implies. Kept
    /// side-effect-free on `self.events` apart from the pushes themselves.
    fn emit_frame_events(&mut self, prev: &ScreenState, next: &ScreenState) {
        let structure_changed = prev.structure_hash != next.structure_hash;
        let visual_changed = prev.visual_hash != next.visual_hash;
        if structure_changed {
            self.push_event(crate::events::TerminalEventKind::ScreenChanged {
                dirty_rows: dirty_rows(prev, next),
            });
        } else if visual_changed {
            self.push_event(crate::events::TerminalEventKind::VisualChanged);
        }
        if prev.cursor != next.cursor {
            self.push_event(crate::events::TerminalEventKind::CursorMoved {
                x: next.cursor.x,
                y: next.cursor.y,
            });
        }
        if prev.title != next.title {
            if let Some(t) = &next.title {
                self.push_event(crate::events::TerminalEventKind::TitleChanged {
                    title: t.clone(),
                });
            }
        }
        // Audit finding 28: the event contract advertises FocusChanged and
        // SemanticChanged; emit them from the same semantic comparison the
        // rest of the engine trusts, instead of leaving advertised
        // vocabulary nothing ordinary observation produces.
        {
            let mut cache = self.semantic.cache().borrow_mut();
            emit_semantic_frame_events(
                self.event_state.queue_mut(),
                &self.id,
                self.generation,
                prev,
                next,
                &mut cache,
            );
        }
        if prev.process.running && !next.process.running {
            self.push_event(crate::events::TerminalEventKind::ProcessExited {
                exit_code: next.process.exit_code,
                exit_signal: next.process.exit_signal.clone(),
            });
        }
    }

    /// Append to the session's event queue.
    fn push_event(&mut self, kind: crate::events::TerminalEventKind) {
        self.event_state
            .queue_mut()
            .push(&self.id, self.generation, kind);
    }

    /// Fold new native-channel events (focus / activate / coverage / any
    /// app-declared verb) into the session event queue (re-review P1: the
    /// unified event substrate). Idempotent per generation — an event is
    /// absorbed exactly once, tracked by the channel's monotonic native_seq,
    /// NOT by a Vec index: once the channel's ring hits its 1024-event cap,
    /// len stays constant while old events evict, and a positional cursor
    /// would permanently believe there is nothing new (re-review P0, the
    /// long-run absorption stall). A restart resets the channel with the
    /// session, so the cursor (reset to 0) stays aligned.
    fn absorb_native_events(&mut self) {
        let cursor = self.event_state.native_absorbed_seq();
        let fresh = self.native.events_since(cursor);
        if fresh.is_empty() {
            return;
        }
        for ev in &fresh {
            self.event_state.queue_mut().push(
                &self.id,
                self.generation,
                crate::events::TerminalEventKind::NativeEvent {
                    event: ev.event.clone(),
                    target: ev.target.clone(),
                },
            );
        }
        let seq = self.native.last_native_seq();
        self.event_state.set_native_absorbed_seq(seq);
    }

    /// Wait, returning the full [`WaitOutcome`] — MCP and audit callers get
    /// reason/elapsed/sequence/state, not just a bool (audit item 5).
    ///
    /// audit finding 37 (preflight): a condition the backend intrinsically
    /// cannot satisfy fails immediately as `Unsupported` rather than burning
    /// its budget in a timeout.
    pub fn wait(&mut self, cond: WaitCond, budget_ms: u64) -> anyhow::Result<WaitOutcome> {
        let caps = self.backend.capabilities();
        caps.require_wait(&cond)?;
        let out = self
            .backend
            .wait(cond, std::time::Duration::from_millis(budget_ms))?;
        Ok(out)
    }

    /// Action-anchored wait: capture this session's event state *before*
    /// sending the action, then call this. See [`TerminalBackend::wait_after`].
    ///
    /// audit finding 37 (preflight): symmetric fast-fail on intrinsically
    /// unsupported conditions.
    pub fn wait_after(
        &mut self,
        baseline: TerminalEventState,
        cond: WaitCond,
        budget_ms: u64,
    ) -> anyhow::Result<WaitOutcome> {
        let caps = self.backend.capabilities();
        caps.require_wait(&cond)?;
        let out =
            self.backend
                .wait_after(baseline, cond, std::time::Duration::from_millis(budget_ms))?;
        Ok(out)
    }
}

#[cfg(test)]
mod emission_tests {
    use super::*;

    /// Audit finding 28: FocusChanged and SemanticChanged are advertised
    /// event vocabulary — the frame diff must actually emit them, and a
    /// pure pixel change must NOT emit a false SemanticChanged.
    #[test]
    fn frame_diff_emits_focus_and_semantic_events() {
        // Two frames that differ only in which line carries the focused
        // marker: the analyzer's focus must move between distinct labels.
        // A "[ Save ]" button plus a reverse-video focus indicator line.
        let base = |focused: u16| {
            let mut s = ScreenState::new(40, 6);
            s.viewport_text = vec![
                "Menu".into(),
                "[ Save ]".into(),
                "[ Quit ]".into(),
                String::new(),
                String::new(),
                String::new(),
            ];
            // The analyzer's focus heuristic #2: the cursor sitting on a
            // control IS the focus.
            s.cursor.y = focused;
            s.cursor.x = 3;
            s
        };
        let a = base(1);
        let b = base(2);
        let sem_a = crate::semantic::analyze(&a);
        let sem_b = crate::semantic::analyze(&b);
        // Premise: the analyzer distinguishes the focus targets.
        assert_ne!(
            sem_a.focus.control_id, sem_b.focus.control_id,
            "premise: the analyzer must see the focus move: {:?} vs {:?}",
            sem_a.focus.control_id, sem_b.focus.control_id
        );

        let mut q = crate::events::TerminalEventQueue::new();
        let mut cache = crate::semantic::SemanticCache::new();
        emit_semantic_frame_events(&mut q, "s", 1, &a, &b, &mut cache);
        let batch = q.since(0);
        assert!(
            batch.events.iter().any(|e| matches!(
                &e.kind,
                crate::events::TerminalEventKind::FocusChanged { .. }
            )),
            "a focus move must emit FocusChanged: {:?}",
            batch
                .events
                .iter()
                .map(|e| e.kind.name())
                .collect::<Vec<_>>()
        );

        // Semantic change: a control appears.
        let mut c = base(1);
        c.viewport_text[3] = "[ New Button ]".into();
        let mut q2 = crate::events::TerminalEventQueue::new();
        emit_semantic_frame_events(&mut q2, "s", 1, &a, &c, &mut cache);
        let batch2 = q2.since(0);
        assert!(
            batch2
                .events
                .iter()
                .any(|e| { matches!(&e.kind, crate::events::TerminalEventKind::SemanticChanged) }),
            "an added control must emit SemanticChanged: {:?}",
            batch2
                .events
                .iter()
                .map(|e| e.kind.name())
                .collect::<Vec<_>>()
        );

        // Honest negative: identical semantics (same screen twice) emits
        // NEITHER event.
        let mut q3 = crate::events::TerminalEventQueue::new();
        emit_semantic_frame_events(&mut q3, "s", 1, &a, &a.clone(), &mut cache);
        assert!(
            q3.since(0).events.is_empty(),
            "identical frames must emit no semantic events"
        );
    }
}
