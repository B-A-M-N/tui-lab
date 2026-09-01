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
    // Future engines (TuiTest fixture, tmux attach) are documented in the
    // audit; they gain variants when they exist — not before.
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
            other => {
                return Err(anyhow::anyhow!(
                    "unknown backend '{other}' (supported: auto, portable_vt100, cli, line_cli, pipe)"
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
        }
    }

    /// Build the engine. The ONE construction site (make_backend folded in).
    fn build(self, cols: u16, rows: u16) -> anyhow::Result<Box<dyn TerminalBackend>> {
        Ok(match self {
            BackendKind::PortableVt => Box::new(PortablePtyBackend::new(cols, rows)),
            BackendKind::PtyLine => Box::new(PtyLineBackend::new(cols, rows)),
            BackendKind::Pipe => Box::new(crate::backend::pipe::PipeBackend::new(cols, rows)),
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
    pub backend_kind: BackendKind,
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
    /// Per-frame semantic cache (Wave G review): repeated `semantic::analyze`
    /// on an unchanged frame (same `structure_hash`) is served from cache
    /// instead of re-running all seven detectors. Interior-mutated by the
    /// read-only `analyze_frame` path.
    semantic_cache: std::cell::RefCell<crate::semantic::SemanticCache>,
    /// Wave F items 58–63: the NativeSemanticProtocol side channel. The
    /// session creates it at start and injects `TUI_LAB_SEMANTIC` into the
    /// child's env; cooperative apps write their real semantic tree there
    /// and observation merges it over inference.
    native: crate::semantic::native::NativeChannel,
    /// How many native-channel events have already been folded into the
    /// session event queue (re-review P1: mode-independent ingestion).
    native_events_absorbed_seq: u64,
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
    pub fn new(id: String, command: String) -> Self {
        Session {
            id,
            command,
            backend_kind: BackendKind::PortableVt,
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
            semantic_cache: std::cell::RefCell::new(crate::semantic::SemanticCache::new()),
            native: crate::semantic::native::NativeChannel::default(),
            native_events_absorbed_seq: 0,
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
    pub fn isolation_evidence(&self) -> Option<&crate::session::isolation::IsolationEvidence> {
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

    /// Evidence-backed terminal profile for this session (Wave G review P1/P2
    /// 16): lifts the live [`Capabilities`] into per-feature rows that each
    /// carry the observation that proved (or failed to prove) the capability,
    /// so an agent never assumes mouse/kitty/title that was never negotiated.
    pub fn terminal_profile(&mut self) -> crate::terminal::TerminalProfile {
        let caps = self.capabilities();
        // The backend enumerates observation from the live backend, so no
        // foreign evidence map is needed here.
        crate::terminal::TerminalProfile::build(
            caps,
            &std::collections::HashMap::new(),
        )
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
        self.start_generation(spec, self.generation)
    }

    /// Start the target as an EXPLICIT generation. `restart()` reserves the
    /// next generation BEFORE launching, so every generation-keyed artifact —
    /// scratch dirs, native channel reset, ProcessStarted, isolation
    /// evidence, run correlation — belongs to the generation that actually
    /// runs, not to the one being replaced (re-review P0: the old order
    /// launched generation N+1 under generation N's identity, then bumped).
    /// A failed launch leaves the old generation as history; nothing is
    /// silently re-used or rolled back.
    pub fn start_generation(&mut self, spec: LaunchSpec, generation: u32) -> anyhow::Result<()> {
        // Exact engine selection (re-review P0): compare parsed kinds, not
        // string classes — pipe vs portable was invisible to the boolean
        // test, so a requested pipe engine never got built on a fresh
        // session. First start always builds the requested engine.
        let requested = BackendKind::parse(&spec.backend)?;
        if requested != self.backend_kind {
            self.backend = requested.build(spec.cols, spec.rows)?;
            self.backend_kind = requested;
        }
        // THIS generation is now the live one — everything below keys on it.
        self.generation = generation;
        // Wave G item 77: apply the isolation profile. The declared string
        // is parsed here (invalid names fail loudly at launch, not silently
        // downgrade); the effective env is profile-filtered, strict wraps
        // the command in `unshare --net --` when the platform provides it,
        // and the backend is told to drop inherited env for clean/strict.
        let isolation = crate::session::isolation::Isolation::parse(&spec.isolation)
            .map_err(|e| anyhow::anyhow!(e))?;
        // Session-unique scratch HOME/TMPDIR keyed by (id, generation).
        let effective_env = isolation.effective_env(&self.id, self.generation, &spec.env);
        let (command, args, wrapper_available, network_isolated) =
            isolation.apply_to_command(&spec.command, &spec.args);
        let requested_network_ns = matches!(
            isolation,
            crate::session::isolation::Isolation::Strict
        );
        self.backend.set_clear_env(!matches!(
            isolation,
            crate::session::isolation::Isolation::Local
        ));
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
        // Native event absorption restarts with the channel (re-review P1:
        // counts stay aligned across generations).
        self.native_events_absorbed_seq = 0;
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
        // `launch_succeeded` is set only here, after `backend.start()` above
        // returned Ok: the wrapper (if any) actually brought the child up.
        self.isolation_evidence = Some(crate::session::isolation::IsolationEvidence::for_profile(
            isolation,
            requested_network_ns,
            wrapper_available,
            network_isolated,
            true,
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
        // The process-start event belongs to the generation start, not the
        // first observation: `last` is seeded here, so an observe()-only
        // emission would never fire it.
        self.push_event(crate::events::TerminalEventKind::ProcessStarted);
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
        // Reserve the NEW generation BEFORE the launch: scratch dirs, the
        // native channel reset, ProcessStarted, and isolation evidence are
        // all keyed on generation at start time. The old order launched N+1
        // under N's identity and bumped afterwards.
        let next = self.generation.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("generation counter exhausted for session '{}'", self.id)
        })?;
        match self.start_generation(spec, next) {
            Ok(()) => Ok(()),
            Err(e) => {
                // The failed launch is recorded as history: generation `next`
                // was consumed by a launch that did not come up. Do NOT
                // silently retry under the old number (evidence would collide).
                Err(e)
            }
        }
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
        // stay pending). Native events (focus/activate/coverage) fold into
        // the session event queue HERE — mode-independent (re-review P1:
        // coverage accounting and native facts must not depend on which
        // observe mode a caller chose).
        self.native.poll();
        self.absorb_native_events();
        let s = self
            .backend
            .observe(std::time::Duration::from_millis(idle_ms))?;
        // Take the previous last ONCE: it becomes both the diff base for
        // event emission and the new `previous`. (A second `take()` on the
        // now-empty slot returned `None` every time — emit_frame_events was
        // dead and every observation pushed a spurious ProcessStarted.)
        match self.last.take() {
            Some(p) => {
                self.emit_frame_events(&p, &s.screen);
                self.previous = Some(p);
            }
            None => {
                // First observation of a generation: process started.
                self.push_event(crate::events::TerminalEventKind::ProcessStarted);
            }
        }
        self.last = Some(s.screen.clone());
        Ok(s.screen)
    }

    /// Wave F items 58–63: the session's native channel snapshot (or None
    /// when the app never wrote one).
    pub fn native_channel(&self) -> &crate::semantic::native::NativeChannel {
        &self.native
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
        self.events.push(&self.id, self.generation, kind);
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
        let cursor = self.native_events_absorbed_seq;
        let fresh = self.native.events_since(cursor);
        if fresh.is_empty() {
            return;
        }
        for ev in &fresh {
            self.events.push(
                &self.id,
                self.generation,
                crate::events::TerminalEventKind::NativeEvent {
                    event: ev.event.clone(),
                    target: ev.target.clone(),
                },
            );
        }
        self.native_events_absorbed_seq = self.native.last_native_seq();
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

    /// The session event queue's last sequence number (highest seq assigned
    /// so far, 0 when empty). Used to anchor event-based completion so an
    /// event firing immediately after the send is never missed.
    pub fn event_queue_last_seq(&self) -> u64 {
        self.events.last_seq()
    }

    /// Semantic analysis of the last settled frame, served from the per-session
    /// [`crate::semantic::SemanticCache`]. Returns `None` when no observation
    /// has happened yet. The cache is keyed on `ScreenState::structure_hash`, so
    /// an unchanged frame returns instantly instead of re-running all seven
    /// detectors (Wave G review P1 15/16).
    pub fn analyze_frame(&self) -> Option<crate::semantic::CacheResult> {
        let screen = self.last()?;
        Some(self.semantic_cache.borrow_mut().analyze(screen))
    }

    /// Structural-cache health (re-review item 41): hit counts surfaced on
    /// summary so the cache's work is evidence, not folklore.
    pub fn semantic_cache_hits(&self) -> u64 {
        self.semantic_cache.borrow().hits()
    }
    pub fn semantic_cache_misses(&self) -> u64 {
        self.semantic_cache.borrow().misses()
    }
    pub fn semantic_cache_hit_rate(&self) -> f32 {
        self.semantic_cache.borrow().hit_rate()
    }

    /// The fused semantic truth for the last settled frame (re-review
    /// Wave-4): one cached detection pass producing both shapes, then the
    /// native overlay applied fresh. Every semantic-bearing observe mode
    /// (summary / semantic / tree / nodes) and the `tui://` semantic
    /// resource routes through here, so they cannot disagree with each
    /// other. Returns `None` when no observation has happened yet.
    pub fn fused_frame(
        &self,
    ) -> Option<(
        crate::semantic::SemanticScreen,
        crate::semantic::node::SemanticTree,
        crate::semantic::native::NativeOverlayReport,
    )> {
        let screen = self.last()?;
        let (sem, tree, report) =
            crate::semantic::fuse(screen, &mut self.semantic_cache.borrow_mut(), &self.native);
        Some((sem, tree, report))
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
        let (sem, _tree, _report) =
            crate::semantic::fuse(screen, &mut self.semantic_cache.borrow_mut(), &self.native);
        sem
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
    pub fn backend_search(
        &mut self,
        query: &str,
    ) -> anyhow::Result<Vec<crate::backend::SearchHit>> {
        Ok(self.backend.search(query)?)
    }

    /// Wave F item 54: OSC 133 shell-integration state (None when the
    /// application emits no shell integration).
    pub fn backend_command_state(&mut self) -> Option<crate::backend::CommandState> {
        self.backend.command_state()
    }

    pub fn backend_version(&self) -> &'static str {
        match self.backend_kind {
            BackendKind::PortableVt => "portable-pty+vt100/0.1",
            BackendKind::PtyLine => "line-cli+lines/0.1",
            BackendKind::Pipe => "pipe/0.1",
        }
    }
}
