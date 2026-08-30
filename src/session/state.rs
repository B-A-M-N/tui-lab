//! A single TUI session: owns a backend and the most recent screen snapshot.
//!
//! A session is now durable (spec section 11/13): it stores the exact
//! [`LaunchSpec`] used to start the target, so a `restart()` relaunches the
//! *same* program with the *same* args/cwd/env/dimensions — and keeps the same
//! session id while bumping `generation`, because a restart is the next
//! generation of the same logical session, not a replacement.

use crate::backend::{Capabilities, PortablePtyBackend, TerminalBackend};
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
    caps: Capabilities,
    last: Option<ScreenState>,
    /// Optional asciicast recorder for capturing the full PTY byte stream.
    recorder: Option<AsciicastRecorder>,
    record_input: bool,
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
            caps: Capabilities::default(),
            last: None,
            recorder: None,
            record_input: false,
        }
    }

    /// Enable recording for this session.
    pub fn enable_recording(&mut self, record_input: bool) {
        self.record_input = record_input;
        let cols = self.cols();
        let rows = self.rows();
        self.recorder = Some(AsciicastRecorder::new(cols, rows, record_input));
    }

    /// Get the recorder for this session.
    fn recorder(&mut self) -> Option<&mut AsciicastRecorder> {
        self.recorder.as_mut()
    }

    /// Get the terminal columns.
    pub fn cols(&self) -> u16 {
        self.launch.as_ref().map(|s| s.cols).unwrap_or(80)
    }

    /// Get the terminal rows.
    pub fn rows(&self) -> u16 {
        self.launch.as_ref().map(|s| s.rows).unwrap_or(24)
    }

    /// Record an output event from the PTY.
    pub fn record_output(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recorder() {
            rec.record_output(bytes);
        }
    }

    /// Record an input event.
    pub fn record_input(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recorder() {
            rec.record_input(bytes);
        }
    }

    /// Record a resize event.
    pub fn record_resize(&mut self, cols: u16, rows: u16) {
        if let Some(rec) = self.recorder() {
            rec.record_resize(cols, rows);
        }
    }

    /// Get the recorded events as NDJSON lines.
    pub fn recording_ndjson(&self) -> Vec<String> {
        self.recorder
            .as_ref()
            .map(|r| r.to_ndjson())
            .unwrap_or_default()
    }

    /// Write recording to a `.cast` file.
    pub fn write_recording<P: AsRef<std::path::Path>>(&self, path: P) -> std::io::Result<()> {
        if let Some(rec) = &self.recorder {
            rec.write_to_file(path)
        } else {
            Ok(())
        }
    }

    /// Check if recording is enabled.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Get the number of recorded events.
    pub fn recording_event_count(&self) -> usize {
        self.recorder.as_ref().map(|r| r.event_count()).unwrap_or(0)
    }

    /// Get the recording duration in seconds.
    pub fn recording_duration_secs(&self) -> f64 {
        self.recorder
            .as_ref()
            .map(|r| r.duration_secs())
            .unwrap_or(0.0)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    pub fn launch(&self) -> Option<&LaunchSpec> {
        self.launch.as_ref()
    }

    /// Start (or restart) the target program from a full spec (spec section 11).
    pub fn start_with_spec(&mut self, spec: LaunchSpec) -> anyhow::Result<()> {
        self.backend.start(
            &spec.command,
            &spec.args,
            spec.cwd.as_deref(),
            &spec.env,
            spec.cols,
            spec.rows,
        )?;
        self.caps = self.backend.capabilities();
        self.command = spec.command.clone();
        self.launch = Some(spec);
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

    /// Observe: refresh the screen, store it, return it.
    pub fn observe(&mut self, idle_ms: u64) -> anyhow::Result<ScreenState> {
        let s = self
            .backend
            .observe(std::time::Duration::from_millis(idle_ms))?;
        // Record the raw output bytes if recording is enabled
        if self.is_recording() {
            // Note: The PTY backend already captures bytes; we'd need to hook into
            // the raw byte stream. For now, we record what we can from the ScreenState.
            // A full implementation would intercept bytes in the PTY reader.
            if let Some(rec) = self.recorder.as_mut() {
                // Record text view as output (approximate)
                let text = s.screen.text_view();
                if !text.is_empty() {
                    rec.record_output(text.as_bytes());
                }
            }
        }
        self.last = Some(s.screen.clone());
        Ok(s.screen)
    }

    pub fn last(&self) -> Option<&ScreenState> {
        self.last.as_ref()
    }

    pub fn send(&mut self, input: crate::backend::Input) -> anyhow::Result<()> {
        self.backend.send_input(input)?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.backend.resize(cols, rows)?;
        if let Some(spec) = self.launch.as_mut() {
            spec.cols = cols;
            spec.rows = rows;
        }
        Ok(())
    }

    pub fn wait(&mut self, cond: crate::backend::WaitCond, budget_ms: u64) -> anyhow::Result<bool> {
        let out = self
            .backend
            .wait(cond, std::time::Duration::from_millis(budget_ms))?;
        Ok(out.met)
    }

    pub fn process(&mut self) -> ProcessState {
        self.backend.process()
    }

    pub fn backend_version(&self) -> &'static str {
        "portable-pty+vt100/0.1"
    }
}
