//! Asciinema v3 cast format recorder (spec item 40).
//!
//! Captures the raw PTY byte stream and input events to produce a valid
//! `.cast` file compatible with asciinema player.

use std::fs::File;
use std::io::Write;
use std::path::Path;

/// A single event in the recording.
#[derive(Debug, Clone)]
pub enum RecordingEvent {
    /// Terminal output bytes at a given time.
    Output(Vec<u8>),
    /// Input bytes sent to the terminal at a given time.
    Input(Vec<u8>),
    /// Resize event at a given time.
    Resize(u16, u16),
    /// Marker event.
    Marker(String),
}

/// Recorder that captures events and writes them in asciinema v3 format.
pub struct AsciicastRecorder {
    cols: u16,
    rows: u16,
    events: Vec<(f64, RecordingEvent)>,
    start_time: Option<std::time::Instant>,
    /// Whether to record input events (may contain secrets).
    record_input: bool,
    /// Temporary suppression of input recording (re-review P0 leak fix).
    /// Raised around sends of sensitive payloads so their bytes never reach
    /// the cast file; cleared immediately after. Not serialized —
    /// suppression is per-send, never a durable mode.
    suppress_input: bool,
    /// Temporary suppression of OUTPUT recording (leak fix, echo half).
    /// The tty line discipline echoes typed bytes back as output, so
    /// suppressing the input event alone would still leave the payload in
    /// the cast via the echo. During a sensitive transaction's window both
    /// streams are suppressed; the application's own (masked) rendering
    /// resumes after.
    suppress_output: bool,
    /// Recording provenance (audit finding 57): survives the start
    /// response and lands in the `.cast` extension header.
    boundary: &'static str,
    fidelity: &'static str,
    lossy: bool,
    /// Why this recording is lossy when applicable.
    loss_reason: Option<&'static str>,
}

impl AsciicastRecorder {
    pub fn new(cols: u16, rows: u16, record_input: bool) -> Self {
        Self::with_fidelity(
            cols,
            rows,
            record_input,
            "pty-bytes",
            "raw_pty_stream",
            false,
            None,
        )
    }

    /// Construct with explicit transport provenance (audit finding 57).
    /// `loss_reason` explains what a sampled/reconstructed transport cannot
    /// prove; `None` means lossless at the stated boundary.
    pub fn with_fidelity(
        cols: u16,
        rows: u16,
        record_input: bool,
        boundary: &'static str,
        fidelity: &'static str,
        lossy: bool,
        loss_reason: Option<&'static str>,
    ) -> Self {
        AsciicastRecorder {
            cols,
            rows,
            events: Vec::new(),
            start_time: None,
            record_input,
            suppress_input: false,
            suppress_output: false,
            boundary,
            fidelity,
            lossy,
            loss_reason,
        }
    }

    /// Recording transport provenance for artifact metadata and stop
    /// responses.
    pub fn fidelity_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "boundary": self.boundary,
            "fidelity": self.fidelity,
            "lossy": self.lossy,
            "loss_reason": self.loss_reason,
        })
    }

    /// Raise/lower the input-recording suppression gate (leak fix).
    ///
    /// While suppressed, [`Self::record_input`] is a no-op even when
    /// `record_input` is enabled. Output and resize events are unaffected:
    /// the screen echo a TUI draws in response to a sensitive input is
    /// masked by the application itself (e.g. password fields) and remains
    /// part of the visual record; the raw keystrokes do not.
    pub fn suppress_input(&mut self) {
        self.suppress_input = true;
    }

    pub fn resume_input(&mut self) {
        self.suppress_input = false;
    }

    /// Raise/lower the OUTPUT-recording suppression gate (leak fix, echo
    /// half). While raised, [`Self::record_output`] is a no-op: a tty echo
    /// of sensitive keystrokes is the payload in disguise.
    pub fn suppress_output(&mut self) {
        self.suppress_output = true;
    }

    pub fn resume_output(&mut self) {
        self.suppress_output = false;
    }

    /// Record an output event (terminal bytes). Suppressed while the
    /// output gate is raised (sensitive transaction window).
    pub fn record_output(&mut self, bytes: &[u8]) {
        if self.suppress_output {
            return;
        }
        if self.start_time.is_none() {
            self.start_time = Some(std::time::Instant::now());
        }
        let t = self.elapsed_secs();
        self.events
            .push((t, RecordingEvent::Output(bytes.to_vec())));
    }

    /// Record an input event (bytes sent to terminal). No-op when input
    /// recording is disabled **or** currently suppressed (leak fix: the
    /// suppression gate outranks the `record_input` policy — a sensitive
    /// send must never land in the cast even with input recording on).
    pub fn record_input(&mut self, bytes: &[u8]) {
        if !self.record_input || self.suppress_input {
            return;
        }
        if self.start_time.is_none() {
            self.start_time = Some(std::time::Instant::now());
        }
        let t = self.elapsed_secs();
        self.events.push((t, RecordingEvent::Input(bytes.to_vec())));
    }

    /// Record a resize event.
    pub fn record_resize(&mut self, cols: u16, rows: u16) {
        if self.start_time.is_none() {
            self.start_time = Some(std::time::Instant::now());
        }
        let t = self.elapsed_secs();
        self.events.push((t, RecordingEvent::Resize(cols, rows)));
        self.cols = cols;
        self.rows = rows;
    }

    /// Record a marker.
    pub fn record_marker(&mut self, text: &str) {
        let t = self.elapsed_secs();
        self.events
            .push((t, RecordingEvent::Marker(text.to_string())));
    }

    /// Get the recorded events as NDJSON lines.
    pub fn to_ndjson(&self) -> Vec<String> {
        let mut lines = Vec::new();
        // Header
        // Asciinema permits arbitrary header metadata; keep fidelity in the
        // artifact itself so later consumers need not trust the original
        // start response (audit finding 57).
        let header = serde_json::json!({
            "version": 3,
            "width": self.cols,
            "height": self.rows,
            "timestamp": 0,
            "tui_lab": self.fidelity_metadata(),
        });
        lines.push(header.to_string());

        for (t, event) in &self.events {
            match event {
                RecordingEvent::Output(data) => {
                    let escaped = String::from_utf8_lossy(data);
                    let ev = serde_json::json!([t, "o", escaped]);
                    lines.push(ev.to_string());
                }
                RecordingEvent::Input(data) => {
                    let escaped = String::from_utf8_lossy(data);
                    let ev = serde_json::json!([t, "i", escaped]);
                    lines.push(ev.to_string());
                }
                RecordingEvent::Resize(cols, rows) => {
                    let ev = serde_json::json!([t, "r", format!("{}x{}", cols, rows)]);
                    lines.push(ev.to_string());
                }
                RecordingEvent::Marker(text) => {
                    let ev = serde_json::json!([t, "m", text]);
                    lines.push(ev.to_string());
                }
            }
        }
        lines
    }

    /// Write the recording to a `.cast` file.
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        for line in self.to_ndjson() {
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    /// Get the number of events recorded.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Get the duration of the recording in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.events.last().map(|(t, _)| *t).unwrap_or(0.0)
    }

    fn elapsed_secs(&self) -> f64 {
        self.start_time
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

impl Drop for AsciicastRecorder {
    fn drop(&mut self) {
        // Nothing special needed — events are owned by the struct.
    }
}

/// Privacy/sanitization policy for recordings.
#[derive(Debug, Clone)]
pub struct RecordingPolicy {
    pub record_input: bool,
    pub redact_patterns: Vec<String>,
}

impl Default for RecordingPolicy {
    fn default() -> Self {
        RecordingPolicy {
            record_input: false,
            redact_patterns: vec![
                r"(?i)password\s*[:=]\s*\S+".to_string(),
                r"(?i)secret\s*[:=]\s*\S+".to_string(),
                r"(?i)token\s*[:=]\s*\S+".to_string(),
                r"(?i)key\s*[:=]\s*\S+".to_string(),
            ],
        }
    }
}

impl RecordingPolicy {
    /// Apply redaction to a string.
    pub fn redact(&self, text: &str) -> String {
        let mut result = text.to_string();
        for pattern in &self.redact_patterns {
            if let Ok(re) = regex::Regex::new(pattern) {
                result = re.replace_all(&result, "[REDACTED]").to_string();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fidelity_metadata_survives_cast_header() {
        let mut sampled = AsciicastRecorder::with_fidelity(
            40,
            10,
            false,
            "rendered-pane-snapshots",
            "rendered_snapshots",
            true,
            Some("sampled"),
        );
        sampled.record_output(b"hello");
        let lines = sampled.to_ndjson();
        let header: serde_json::Value =
            serde_json::from_str(lines.first().expect("header")).expect("json");
        assert_eq!(header["tui_lab"]["boundary"], "rendered-pane-snapshots");
        assert_eq!(header["tui_lab"]["fidelity"], "rendered_snapshots");
        assert_eq!(header["tui_lab"]["lossy"], true);
        assert_eq!(header["tui_lab"]["loss_reason"], "sampled");

        let raw = AsciicastRecorder::new(80, 24, false);
        assert_eq!(raw.fidelity_metadata()["lossy"], false);
    }

    #[test]
    fn test_recorder_basic() {
        let mut rec = AsciicastRecorder::new(80, 24, false);
        rec.record_output(b"Hello World");
        rec.record_resize(120, 40);
        rec.record_marker("test marker");

        let lines = rec.to_ndjson();
        assert!(lines.len() >= 4); // header + 3 events
        assert!(lines[0].contains("\"version\":3"));
    }

    #[test]
    fn test_recorder_with_input() {
        let mut rec = AsciicastRecorder::new(80, 24, true);
        rec.record_output(b"prompt> ");
        rec.record_input(b"ls\n");

        let lines = rec.to_ndjson();
        assert_eq!(lines.len(), 3); // header + 2 events
    }

    #[test]
    fn test_recorder_no_input() {
        let mut rec = AsciicastRecorder::new(80, 24, false);
        rec.record_output(b"prompt> ");
        rec.record_input(b"ls\n"); // should be ignored

        let lines = rec.to_ndjson();
        assert_eq!(lines.len(), 2); // header + 1 event (input ignored)
    }

    /// The suppression gate outranks `record_input=true`: a sensitive send's
    /// bytes must never reach the cast (re-review P0 leak fix, recording
    /// half). Normal input before/after the gate is still recorded.
    /// The output gate blocks tty echo of a sensitive payload (leak fix):
    /// the echo IS the payload in disguise.
    #[test]
    fn test_recorder_output_gate_blocks_echo() {
        let mut rec = AsciicastRecorder::new(80, 24, true);
        rec.record_output(b"prompt> ");
        rec.suppress_output();
        rec.record_output(b"super-secret-password\r\n");
        rec.resume_output();
        rec.record_output(b"after\r\n");

        let body = rec.to_ndjson().join("\n");
        assert!(body.contains("prompt>"));
        assert!(body.contains("after"));
        assert!(
            !body.contains("super-secret-password"),
            "echoed sensitive output must not leak into the cast"
        );
    }

    #[test]
    fn test_recorder_suppression_gate_blocks_sensitive_input() {
        let mut rec = AsciicastRecorder::new(80, 24, true);
        rec.record_input(b"normal\n");
        rec.suppress_input();
        rec.record_input(b"super-secret-password\n");
        rec.resume_input();
        rec.record_input(b"normal-again\n");

        let body = rec.to_ndjson().join("\n");
        assert!(body.contains("normal"), "pre-gate input kept");
        assert!(body.contains("normal-again"), "post-gate input kept");
        assert!(
            !body.contains("super-secret-password"),
            "suppressed input must not leak into the cast"
        );
    }

    #[test]
    fn test_policy_redaction() {
        let policy = RecordingPolicy::default();
        let text = "password: secret123 token: abc456 normal text";
        let redacted = policy.redact(text);
        assert!(redacted.contains("[REDACTED]"));
        assert!(redacted.contains("normal text"));
    }
}
