//! A single TUI session: owns a backend and the most recent screen snapshot.
//!
//! A session is now durable (spec section 11/13): it stores the exact
//! [`LaunchSpec`] used to start the target, so a `restart()` relaunches the
//! *same* program with the *same* args/cwd/env/dimensions — and keeps the same
//! session id while bumping `generation`, because a restart is the next
//! generation of the same logical session, not a replacement.

use crate::backend::{
    Capabilities, PortablePtyBackend, RecordingHook, RecordingHookSlot, TerminalBackend,
    TerminalEventState, WaitCond, WaitOutcome,
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
            cols, rows, record_input,
        )));
        self.recorder = Some(sink.clone());
        // Attach the hook to the backend so raw bytes flow to the recorder.
        let hook: std::sync::Arc<dyn RecordingHook> = std::sync::Arc::new(RecorderHook {
            sink: sink.clone(),
        });
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

    /// Access the recorder (shared, interior-mutable).
    pub fn recorder(
        &self,
    ) -> Option<&std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>> {
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

    /// Record a resize event (manual path).
    pub fn record_resize(&mut self, cols: u16, rows: u16) {
        if let Some(rec) = self.recorder.as_ref() {
            if let Ok(mut r) = rec.lock() {
                r.record_resize(cols, rows);
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
                let r = rec.lock().map_err(|_| {
                    std::io::Error::other("recorder mutex poisoned")
                })?;
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

    /// Capabilities as they were when the session last (re)started.
    pub fn capabilities_at_start(&self) -> Capabilities {
        self.caps_at_start.clone()
    }

    pub fn launch(&self) -> Option<&LaunchSpec> {
        self.launch.as_ref()
    }

    /// Start (or restart) the target program from a full spec (spec section 11).
    pub fn start_with_spec(&mut self, spec: LaunchSpec) -> anyhow::Result<()> {
        // Re-attach any active recording hook (the backend was just replaced
        // internally on restart).
        self.backend
            .set_recording_hook(self.recording_slot.clone());
        self.backend.start(
            &spec.command,
            &spec.args,
            spec.cwd.as_deref(),
            &spec.env,
            spec.cols,
            spec.rows,
        )?;
        self.caps_at_start = self.backend.capabilities();
        self.command = spec.command.clone();
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
    /// the previous frame for diffing, and return the new frame.
    pub fn observe(&mut self, idle_ms: u64) -> anyhow::Result<ScreenState> {
        let s = self
            .backend
            .observe(std::time::Duration::from_millis(idle_ms))?;
        self.previous = self.last.take();
        self.last = Some(s.screen.clone());
        Ok(s.screen)
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
        self.backend.resize(cols, rows)?;
        // Record resize at the recorder level as well (the raw hook only sees
        // PTY bytes, not resize intent; audit item 26).
        self.record_resize(cols, rows);
        if let Some(spec) = self.launch.as_mut() {
            spec.cols = cols;
            spec.rows = rows;
        }
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
        let out = self.backend.wait_after(
            baseline,
            cond,
            std::time::Duration::from_millis(budget_ms),
        )?;
        Ok(out)
    }

    pub fn process(&mut self) -> ProcessState {
        self.backend.process()
    }

    pub fn backend_version(&self) -> &'static str {
        "portable-pty+vt100/0.1"
    }
}
