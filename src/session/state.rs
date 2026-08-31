//! A single TUI session: owns a backend and the most recent screen snapshot.
//!
//! A session is now durable (spec section 11/13): it stores the exact
//! [`LaunchSpec`] used to start the target, so a `restart()` relaunches the
//! *same* program with the *same* args/cwd/env/dimensions — and keeps the same
//! session id while bumping `generation`, because a restart is the next
//! generation of the same logical session, not a replacement.

use crate::backend::{
    Capabilities, LineCliBackend, PortablePtyBackend, RecordingHook, RecordingHookSlot,
    TerminalBackend, TerminalEventState, WaitCond, WaitOutcome,
};
use crate::recording::AsciicastRecorder;
use crate::screen::{ProcessState, ScreenState};

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

/// Rows whose cell text differs between two frames (Wave B item 12:
/// `ScreenChanged.dirty_rows` derived at the only place with both frames).
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
    pub backend_kind: String,
    pub generation: u32,
    launch: Option<LaunchSpec>,
    backend: Box<dyn TerminalBackend>,
    /// Capabilities snapshot captured at start. Kept for historical comparison
    /// only — live capability queries go through [`Session::capabilities`],
    /// which re-queries the backend so post-start negotiation (mouse, paste,
    /// title) is visible (audit item 11).
    caps_at_start: Capabilities,
    /// The last *settled* observation (what `observe()` returned).
    last: Option<ScreenState>,
    /// The observation before `last` — set by `observe()` so `mode=diff` and
    /// `diff()` always compare previous→current, never self→self (audit item 12).
    previous: Option<ScreenState>,
    /// Optional asciicast recorder for capturing the full PTY byte stream.
    recorder: Option<std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>>,
    record_input: bool,
    /// Raw PTY byte-stream hook slot; attached to the backend at start.
    recording_slot: RecordingHookSlot,
    /// Monotonic anchor sequence (re-review P1: `ObservationAnchor.index`
    /// must be a real per-session counter, not a hardcoded 0). Allocated by
    /// [`Session::next_anchor`]; shared by every executor path.
    next_anchor_seq: u64,
    /// Per-session terminal event queue (Wave B item 12): every observe,
    /// send, resize, and process-state change appends here. Waits, audits,
    /// incremental observation, and run persistence all read this one
    /// stream.
    events: crate::events::TerminalEventQueue,
    /// Per-consumer observation cursors (Wave B item 13): named consumers
    /// (`hermes`, `audit`, `explorer`, `recording`, ...) each remember their
    /// own position in the event stream. Cursors live in the session so a
    /// reconnecting consumer resumes where it left off, but reading is
    /// stateless — `events_since` never mutates a cursor implicitly.
    cursors: std::collections::HashMap<String, u64>,
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

/// Bridge that feeds raw PTY bytes into the session's [`AsciicastRecorder`].
/// Lives behind `Arc<dyn RecordingHook>` on the backend's reader thread.
struct RecorderHook {
    sink: std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>,
}

impl RecordingHook for RecorderHook {
    fn on_output(&self, bytes: &[u8]) {
        if let Ok(mut rec) = self.sink.lock() {
            rec.record_output(bytes);
        }
    }
    fn on_input(&self, bytes: &[u8]) {
        if let Ok(mut rec) = self.sink.lock() {
            rec.record_input(bytes);
        }
    }
    fn on_resize(&self, cols: u16, rows: u16) {
        if let Ok(mut rec) = self.sink.lock() {
            rec.record_resize(cols, rows);
        }
    }
}

impl Session {
    /// Wave F items 50–51: backend selection. `auto` / `portable_vt100` map
    /// to the PTY engine; `cli` / `line_cli` map to the line CLI engine.
    /// Unknown names are an error — never a silent fallback.
    fn make_backend(kind: &str, cols: u16, rows: u16) -> anyhow::Result<Box<dyn TerminalBackend>> {
        match kind {
            "auto" | "portable_vt100" => Ok(Box::new(PortablePtyBackend::new(cols, rows))),
            "cli" | "line_cli" => Ok(Box::new(LineCliBackend::new(cols, rows))),
            other => Err(anyhow::anyhow!(
                "unknown backend '{}' (supported: auto, portable_vt100, cli)",
                other
            )),
        }
    }

    pub fn new(id: String, command: String) -> Self {
        Session {
            id,
            command,
            backend_kind: "portable-pty+vt100".to_string(),
            generation: 1,
            launch: None,
            backend: Box::new(PortablePtyBackend::new(80, 24)),
            caps_at_start: Capabilities::default(),
            last: None,
            previous: None,
            recorder: None,
            record_input: false,
            recording_slot: crate::backend::new_recording_hook_slot(),
            next_anchor_seq: 0,
            events: crate::events::TerminalEventQueue::new(),
            cursors: std::collections::HashMap::new(),
            native: crate::semantic::native::NativeChannel::default(),
            lease: crate::session::lease::LeaseState::default(),
            isolation_evidence: None,
        }
    }

    /// Wave G item 76: take the human control lease (holder + TTL). A live
    /// lease refuses with the current holder's grant.
    pub fn acquire_lease(
        &mut self,
        holder: &str,
        ttl_ms: u64,
    ) -> Result<crate::session::lease::ControlLease, crate::session::lease::ControlLease> {
        self.lease.acquire(holder, ttl_ms)
    }

    /// Release the control lease. Returns whether one was live.
    pub fn release_lease(&mut self) -> bool {
        self.lease.release()
    }

    /// The active lease, if any.
    pub fn active_lease(&mut self) -> Option<crate::session::lease::ControlLease> {
        self.lease.active()
    }

    /// Wave G item 76: does the lease currently block machine driving?
    /// Every driving path checks this before sending input.
    pub fn driving_blocked(&mut self) -> Option<crate::session::lease::ControlLease> {
        self.lease.active()
    }

    /// Wave G item 77: what the current generation actually launched with.
    pub fn isolation_evidence(
        &self,
    ) -> Option<&crate::session::isolation::IsolationEvidence> {
        self.isolation_evidence.as_ref()
    }

    /// Allocate the next monotonic anchor index for this session
    /// (re-review P1 fix 9). Every [`crate::execution::ObservationAnchor`]
    /// created against this session gets a distinct, increasing index.
    pub fn next_anchor(&mut self) -> u64 {
        let n = self.next_anchor_seq;
        self.next_anchor_seq += 1;
        n
    }

    /// Suppress input recording for the duration of `f` (re-review P0 leak
    /// fix, recording half). Sensitive payloads must not reach the cast file
    /// through [`RecordingHook::on_input`]; the hook forwards bytes
    /// synchronously on the calling thread inside `send_input`, so a guard
    /// flag set/cleared around the send is race-free.
    pub fn send_unrecorded(&mut self, input: crate::backend::Input) -> anyhow::Result<()> {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.suppress_input();
            }
        }
        let result = self.backend.send_input(input);
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resume_input();
            }
        }
        result.map_err(anyhow::Error::from)
    }

    /// Raise BOTH recording suppression gates (input + output) for the
    /// duration of a sensitive transaction (leak fix, echo half): the tty
    /// line discipline echoes typed bytes back as output, so the input gate
    /// alone would still leave the payload in the cast. The caller MUST
    /// call [`Self::resume_recording`] when the transaction window closes.
    /// Panics are not expected mid-window (no user code runs here), but a
    /// resumed-late recorder only ever over-suppresses, never leaks.
    pub fn suppress_recording(&mut self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.suppress_input();
                r.suppress_output();
            }
        }
    }

    /// Release both recording suppression gates (sensitive window closed).
    pub fn resume_recording(&mut self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resume_input();
                r.resume_output();
            }
        }
    }

    /// Enable recording for this session.
    ///
    /// The recorder is attached at the raw PTY byte boundary via a
    /// [`RecordingHook`] (audit item 24): every chunk the reader thread reads
    /// is recorded with its timestamp, preserving escape sequences, timing and
    /// intermediate frames. Input bytes are recorded only when
    /// `record_input` is set.
    pub fn enable_recording(&mut self, record_input: bool) {
        self.record_input = record_input;
        let cols = self.cols();
        let rows = self.rows();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(AsciicastRecorder::new(
            cols,
            rows,
            record_input,
        )));
        self.recorder = Some(sink.clone());
        // Attach the hook to the backend so raw bytes flow to the recorder.
        let hook: std::sync::Arc<dyn RecordingHook> =
            std::sync::Arc::new(RecorderHook { sink: sink.clone() });
        *self.recording_slot.lock().expect("recording slot") = Some(hook);
        self.backend.set_recording_hook(self.recording_slot.clone());
    }

    /// Detach recording (stop capturing further bytes; existing events remain).
    pub fn disable_recording(&mut self) {
        self.recorder = None;
        if let Ok(mut slot) = self.recording_slot.lock() {
            *slot = None;
        }
    }

    /// Stop recording and return the recorder so the caller can export it.
    ///
    /// Detaches the PTY hook first (no further events), then `take()`s the
    /// sink — unlike `disable_recording`, which drops it, this hands the
    /// completed recording back to the caller (audit re-review item 2: stop
    /// must not destroy the recorder before retrieval).
    pub fn stop_recording(
        &mut self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>> {
        // Detach the hook so the reader thread stops feeding events.
        if let Ok(mut slot) = self.recording_slot.lock() {
            *slot = None;
        }
        self.recorder.take()
    }

    /// Access the recorder (shared, interior-mutable).
    pub fn recorder(&self) -> Option<&std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>> {
        self.recorder.as_ref()
    }

    /// Get the terminal columns.
    pub fn cols(&self) -> u16 {
        self.launch.as_ref().map(|s| s.cols).unwrap_or(80)
    }

    /// Get the terminal rows.
    pub fn rows(&self) -> u16 {
        self.launch.as_ref().map(|s| s.rows).unwrap_or(24)
    }

    /// Record an output event from the PTY (manual path; the raw-byte hook is
    /// preferred and attached by `enable_recording`).
    pub fn record_output(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recorder.as_ref() {
            if let Ok(mut r) = rec.lock() {
                r.record_output(bytes);
            }
        }
    }

    /// Record an input event (manual path).
    pub fn record_input(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recorder.as_ref() {
            if let Ok(mut r) = rec.lock() {
                r.record_input(bytes);
            }
        }
    }

    /// Get the recorded events as NDJSON lines.
    pub fn recording_ndjson(&self) -> Vec<String> {
        match self.recorder.as_ref() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.to_ndjson(),
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Write recording to a `.cast` file.
    pub fn write_recording<P: AsRef<std::path::Path>>(&self, path: P) -> std::io::Result<()> {
        match self.recorder.as_ref() {
            Some(rec) => {
                let r = rec
                    .lock()
                    .map_err(|_| std::io::Error::other("recorder mutex poisoned"))?;
                r.write_to_file(path)
            }
            None => Ok(()),
        }
    }

    /// Check if recording is enabled.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Get the number of recorded events.
    pub fn recording_event_count(&self) -> usize {
        match self.recorder.as_ref() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.event_count(),
                Err(_) => 0,
            },
            None => 0,
        }
    }

    /// Get the recording duration in seconds.
    pub fn recording_duration_secs(&self) -> f64 {
        match self.recorder.as_ref() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.duration_secs(),
                Err(_) => 0.0,
            },
            None => 0.0,
        }
    }

    /// Live capability query: re-asks the backend so capabilities negotiated
    /// *after* start (mouse, bracketed paste, title) are visible (audit
    /// item 11). `capabilities_at_start()` keeps the historical snapshot.
    pub fn capabilities(&mut self) -> Capabilities {
        self.backend.capabilities()
    }

    /// Item 48: apply a project contract's normalization policy to this
    /// session's backend. Every subsequent structure hash normalizes the
    /// contract's `volatile_patterns` on top of the built-in classes, so a
    /// clock the contract declares volatile stops fragmenting the state
    /// graph. Invalid patterns were rejected at contract load; this only
    /// receives compiled policies.
    pub fn set_normalization_policy(
        &mut self,
        policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
        self.backend.set_normalization_policy(policy);
    }

    /// Capabilities as they were when the session last (re)started.
    pub fn capabilities_at_start(&self) -> Capabilities {
        self.caps_at_start.clone()
    }

    pub fn launch(&self) -> Option<&LaunchSpec> {
        self.launch.as_ref()
    }

    /// Start (or restart) the target program from a full spec (spec section 11).
    pub fn start_with_spec(&mut self, spec: LaunchSpec) -> anyhow::Result<()> {
        // Wave F items 50–51: the LaunchSpec's backend field is now real
        // selection. On restart the engine kind may not change (identity is
        // preserved), but a first start builds the requested engine.
        let wants_line = matches!(spec.backend.as_str(), "cli" | "line_cli");
        let current_is_line = self.backend_kind == "line-cli+lines";
        if wants_line != current_is_line {
            // First start (or engine-kind change): build the requested
            // backend. Recording hook and normalization policy re-attach
            // below.
            self.backend = Self::make_backend(&spec.backend, spec.cols, spec.rows)?;
            self.backend_kind = if wants_line {
                "line-cli+lines".to_string()
            } else {
                "portable-pty+vt100".to_string()
            };
        }
        // Wave G item 77: apply the isolation profile. The declared string
        // is parsed here (invalid names fail loudly at launch, not silently
        // downgrade); the effective env is profile-filtered, strict wraps
        // the command in `unshare --net --` when the platform provides it,
        // and the backend is told to drop inherited env for clean/strict.
        let isolation = crate::session::isolation::Isolation::parse(&spec.isolation)
            .map_err(|e| anyhow::anyhow!(e))?;
        let effective_env = isolation.effective_env(&spec.env);
        let (command, args, network_isolated) =
            isolation.apply_to_command(&spec.command, &spec.args);
        self.backend
            .set_clear_env(!matches!(isolation, crate::session::isolation::Isolation::Local));
        // Re-attach any active recording hook (the backend was just replaced
        // internally on restart).
        self.backend.set_recording_hook(self.recording_slot.clone());
        // Wave F items 58–63: create the native semantic channel for this
        // generation and inject its path into the child's env. Apps that
        // never read TUI_LAB_SEMANTIC are unaffected; cooperative adapters
        // write their real tree there. A channel from a prior generation is
        // reset (same path, fresh content).
        if self.native.path.is_none() {
            if let Ok(ch) = crate::semantic::native::NativeChannel::create() {
                self.native = ch;
            }
        } else {
            self.native.reset();
        }
        let mut effective_env = effective_env; // native channel pair may append
        if let Some(pair) = self.native.env_pair() {
            if !crate::semantic::native::env_has_channel(&effective_env) {
                effective_env.push(pair);
            }
        }
        self.backend.start(
            &command,
            &args,
            spec.cwd.as_deref(),
            &effective_env,
            spec.cols,
            spec.rows,
        )?;
        self.caps_at_start = self.backend.capabilities();
        self.command = command.clone();
        // Evidence of what actually launched (item 77) — kept per generation.
        self.isolation_evidence = Some(crate::session::isolation::IsolationEvidence::for_profile(
            isolation,
            network_isolated,
            &effective_env
                .iter()
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>(),
        ));
        self.launch = Some(spec);
        // A new process generation invalidates prior frame tracking.
        self.previous = None;
        self.last = None;
        let s = self.backend.state()?;
        self.last = Some(s);
        Ok(())
    }

    /// Restart the *same* logical session: reuse the stored [`LaunchSpec`]
    /// (spec section 13). Identity is preserved; generation is incremented so
    /// run artifacts / coverage / scenarios stay correlated.
    pub fn restart(&mut self) -> anyhow::Result<()> {
        let spec = match &self.launch {
            Some(s) => s.clone(),
            None => {
                return Err(anyhow::anyhow!(
                    "cannot restart session '{}': no prior launch spec",
                    self.id
                ))
            }
        };
        self.backend.stop().ok();
        self.start_with_spec(spec)?;
        self.generation += 1;
        Ok(())
    }

    pub fn stop(&mut self) -> anyhow::Result<()> {
        self.backend.stop()?;
        self.last = None;
        Ok(())
    }

    /// Observe: refresh the screen via the backend's event-aware settle, keep
    /// the previous frame for diffing, return the new frame, and emit the
    /// derived events (Wave B item 12): ScreenChanged / VisualChanged /
    /// CursorMoved / TitleChanged / ProcessExited / SemanticChanged-class
    /// transitions are derived by diffing against the previous frame.
    pub fn observe(&mut self, idle_ms: u64) -> anyhow::Result<ScreenState> {
        // Wave F items 58–63: drain any native semantic frames the app has
        // written since the last observation (bounded read, partial lines
        // stay pending).
        self.native.poll();
        let s = self
            .backend
            .observe(std::time::Duration::from_millis(idle_ms))?;
        self.previous = self.last.take();
        let prev = self.last.take();
        if let Some(p) = &prev {
            self.emit_frame_events(p, &s.screen);
        } else {
            // First observation of a generation: process started.
            self.push_event(crate::events::TerminalEventKind::ProcessStarted);
        }
        self.last = Some(s.screen.clone());
        Ok(s.screen)
    }

    /// Wave F items 58–63: the session's native channel snapshot (or None
    /// when the app never wrote one).
    pub fn native_channel(&self) -> &crate::semantic::native::NativeChannel {
        &self.native
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
        if prev.process.running && !next.process.running {
            self.push_event(crate::events::TerminalEventKind::ProcessExited {
                exit_code: next.process.exit_code,
                exit_signal: next.process.exit_signal.clone(),
            });
        }
    }

    /// Append to the session's event queue.
    fn push_event(&mut self, kind: crate::events::TerminalEventKind) {
        self.events
            .push(&self.id, self.generation, kind);
    }

    /// Read events after `cursor` WITHOUT moving it (per-consumer cursors,
    /// Wave B item 13: the caller owns the position).
    pub fn events_since(&self, cursor: u64) -> crate::events::EventBatch {
        self.events.since(cursor)
    }

    /// Read events after the named consumer's stored cursor, then advance
    /// that cursor to the served position. Unknown consumers start at 0.
    pub fn events_for_consumer(&mut self, consumer: &str) -> crate::events::EventBatch {
        let cursor = self.cursors.get(consumer).copied().unwrap_or(0);
        let batch = self.events.since(cursor);
        self.cursors.insert(consumer.to_string(), batch.cursor);
        batch
    }

    /// The session's whole retained event stream, plus declared-gap stats.
    pub fn event_queue_stats(&self) -> serde_json::Value {
        serde_json::json!({
            "total": self.events.total(),
            "retained": self.events.retained(),
            "evicted": self.events.evicted(),
            "last_seq": self.events.last_seq(),
        })
    }

    /// Drain retained events for run persistence (keeps seq continuity).
    pub fn drain_events(&mut self) -> Vec<crate::events::TerminalEvent> {
        self.events.drain()
    }

    /// The most recent observation.
    pub fn last(&self) -> Option<&ScreenState> {
        self.last.as_ref()
    }

    /// The observation before `last`, if two observations have been made since
    /// the last (re)start. Used for real previous→current diffs (audit item 12).
    pub fn previous(&self) -> Option<&ScreenState> {
        self.previous.as_ref()
    }

    /// Current event sequence state (for action-anchored waits).
    pub fn event_state(&self) -> TerminalEventState {
        self.backend.event_state()
    }

    /// Send input. When recording, the backend has already delivered the exact
    /// encoded bytes to the recording hook, so the cast shows the real bytes
    /// (audit item 25).
    pub fn send(&mut self, input: crate::backend::Input) -> anyhow::Result<()> {
        self.backend.send_input(input)?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // Backend owns resize recording (re-review item 3): the hook fires
        // only after the PTY resize actually succeeded, so a failed resize
        // is never recorded. The Session-level record here was a duplicate.
        self.backend.resize(cols, rows)?;
        if let Some(spec) = self.launch.as_mut() {
            spec.cols = cols;
            spec.rows = rows;
        }
        self.push_event(crate::events::TerminalEventKind::Resize { cols, rows });
        Ok(())
    }

    /// Wait, returning the full [`WaitOutcome`] — MCP and audit callers get
    /// reason/elapsed/sequence/state, not just a bool (audit item 5).
    pub fn wait(&mut self, cond: WaitCond, budget_ms: u64) -> anyhow::Result<WaitOutcome> {
        let out = self
            .backend
            .wait(cond, std::time::Duration::from_millis(budget_ms))?;
        Ok(out)
    }

    /// Action-anchored wait: capture this session's event state *before*
    /// sending the action, then call this. See [`TerminalBackend::wait_after`].
    pub fn wait_after(
        &mut self,
        baseline: TerminalEventState,
        cond: WaitCond,
        budget_ms: u64,
    ) -> anyhow::Result<WaitOutcome> {
        let out =
            self.backend
                .wait_after(baseline, cond, std::time::Duration::from_millis(budget_ms))?;
        Ok(out)
    }

    pub fn process(&mut self) -> ProcessState {
        self.backend.process()
    }

    /// Wave F item 53: true scrollback from the backend.
    pub fn backend_scrollback(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(self.backend.scrollback_lines()?)
    }

    /// Wave F item 53: search viewport + scrollback.
    pub fn backend_search(&mut self, query: &str) -> anyhow::Result<Vec<crate::backend::SearchHit>> {
        Ok(self.backend.search(query)?)
    }

    /// Wave F item 54: OSC 133 shell-integration state (None when the
    /// application emits no shell integration).
    pub fn backend_command_state(&mut self) -> Option<crate::backend::CommandState> {
        self.backend.command_state()
    }

    pub fn backend_version(&self) -> &'static str {
        if self.backend_kind == "line-cli+lines" {
            "line-cli+lines/0.1"
        } else {
            "portable-pty+vt100/0.1"
        }
    }
}
