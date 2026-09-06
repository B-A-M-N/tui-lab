//! Asciicast recording: enable/suppress/stop, raw byte taps, recording persistence.
//!
//! Impl-family extraction (Phase 2): the `Session` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. `Session` remains the single
//! per-session concurrency authority — no independent locking is
//! introduced. Signatures, visibility, and callers are unchanged.

use super::*;

impl Session {
    /// Raise BOTH recording suppression gates (input + output) for the
    /// duration of a sensitive transaction (leak fix, echo half): the tty
    /// line discipline echoes typed bytes back as output, so the input gate
    /// alone would still leave the payload in the cast. The caller MUST
    /// call [`Self::resume_recording`] when the transaction window closes.
    /// Panics are not expected mid-window (no user code runs here), but a
    /// resumed-late recorder only ever over-suppresses, never leaks.
    pub fn suppress_recording(&mut self) {
        self.recording.suppress_all();
    }

    /// Release both recording suppression gates (sensitive window closed).
    pub fn resume_recording(&mut self) {
        self.recording.resume_all();
    }

    /// Enable recording for this session.
    ///
    /// The recorder is attached at the raw PTY byte boundary via a
    /// [`RecordingHook`] (audit item 24): every chunk the reader thread reads
    /// is recorded with its timestamp, preserving escape sequences, timing and
    /// intermediate frames. Input bytes are recorded only when
    /// `record_input` is set.
    pub fn enable_recording(&mut self, record_input: bool) {
        let cols = self.cols();
        let rows = self.rows();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(AsciicastRecorder::new(
            cols,
            rows,
            record_input,
        )));
        self.recording.install(sink.clone(), record_input);
        // The session hook is permanent; swap the recorder INSIDE it so the
        // reader thread's event ingestion is never interrupted.
        self.set_hook_recorder(Some(sink));
    }

    /// Detach recording (stop capturing further bytes; existing events remain).
    pub fn disable_recording(&mut self) {
        self.recording.detach();
        self.set_hook_recorder(None);
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
        // The session hook stays attached (event ingestion continues);
        // only the recorder side stops.
        self.set_hook_recorder(None);
        self.recording.take()
    }

    /// Access the recorder (shared, interior-mutable).
    pub fn recorder(&self) -> Option<&std::sync::Arc<std::sync::Mutex<AsciicastRecorder>>> {
        self.recording.sink()
    }

    /// Record an output event from the PTY (manual path; the raw-byte hook is
    /// preferred and attached by `enable_recording`).
    pub fn record_output(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recording.sink() {
            if let Ok(mut r) = rec.lock() {
                r.record_output(bytes);
            }
        }
    }

    /// Record an input event (manual path).
    pub fn record_input(&mut self, bytes: &[u8]) {
        if let Some(rec) = self.recording.sink() {
            if let Ok(mut r) = rec.lock() {
                r.record_input(bytes);
            }
        }
    }

    /// Get the recorded events as NDJSON lines.
    pub fn recording_ndjson(&self) -> Vec<String> {
        match self.recording.sink() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.to_ndjson(),
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Write recording to a `.cast` file.
    pub fn write_recording<P: AsRef<std::path::Path>>(&self, path: P) -> std::io::Result<()> {
        match self.recording.sink() {
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
        self.recording.active()
    }

    /// Get the number of recorded events.
    pub fn recording_event_count(&self) -> usize {
        match self.recording.sink() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.event_count(),
                Err(_) => 0,
            },
            None => 0,
        }
    }

    /// Get the recording duration in seconds.
    pub fn recording_duration_secs(&self) -> f64 {
        match self.recording.sink() {
            Some(rec) => match rec.lock() {
                Ok(r) => r.duration_secs(),
                Err(_) => 0.0,
            },
            None => 0.0,
        }
    }
}
