//! Asciicast recording state for one session (god-object round 2, G4).
//!
//! The session-side recorder handle and the input-recording flag move
//! OUT of the flat `Session` bucket into this holder. The hook-facing
//! pieces stay on `Session`: the permanent hook slot (shared with the
//! backend's reader thread at attach time) and the hook's recorder
//! interior (`set_hook_recorder`), because swapping those would break
//! the attached hook. This holder owns the RECORDING ITSELF: which sink
//! is active, whether input is recorded, and the suppression-gate rules
//! the sensitive-transaction paths rely on.
//!
//! Every accessor mirrors the exact `Option<Arc<Mutex<Recorder>>>`
//! shape the flat fields had — same locks, same suppression calls, no
//! new locking (invariant 15).

/// The session-side recording state for one session.
pub(crate) struct RecordingState {
    /// Optional asciicast recorder for capturing the full PTY byte stream.
    recorder: Option<std::sync::Arc<std::sync::Mutex<crate::recording::AsciicastRecorder>>>,
    record_input: bool,
}

impl RecordingState {
    pub(crate) fn new() -> Self {
        RecordingState {
            recorder: None,
            record_input: false,
        }
    }

    /// Raise BOTH suppression gates (input + output) for the duration of
    /// a sensitive transaction (leak fix, echo half): the tty line
    /// discipline echoes typed bytes back as output, so the input gate
    /// alone would still leave the payload in the cast.
    pub(crate) fn suppress_all(&self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.suppress_input();
                r.suppress_output();
            }
        }
    }

    /// Release both suppression gates (sensitive window closed).
    pub(crate) fn resume_all(&self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resume_input();
                r.resume_output();
            }
        }
    }

    /// Suppress the input gate only, for `send_unrecorded` (the payload
    /// must not reach the cast; output echo is separately handled by the
    /// caller's all-gate window when needed).
    pub(crate) fn suppress_input_only(&self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.suppress_input();
            }
        }
    }

    /// Release the input gate only.
    pub(crate) fn resume_input_only(&self) {
        if let Some(rec) = &self.recorder {
            if let Ok(mut r) = rec.lock() {
                r.resume_input();
            }
        }
    }

    /// Install the recording sink and its input flag (enable path).
    pub(crate) fn install(
        &mut self,
        sink: std::sync::Arc<std::sync::Mutex<crate::recording::AsciicastRecorder>>,
        record_input: bool,
    ) {
        self.record_input = record_input;
        self.recorder = Some(sink);
    }

    /// Drop the sink (disable path: existing events remain).
    pub(crate) fn detach(&mut self) {
        self.recorder = None;
    }

    /// Take the sink (stop path: hand the completed recording back).
    pub(crate) fn take(
        &mut self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<crate::recording::AsciicastRecorder>>> {
        self.recorder.take()
    }

    /// The active sink, when recording.
    pub(crate) fn sink(
        &self,
    ) -> Option<&std::sync::Arc<std::sync::Mutex<crate::recording::AsciicastRecorder>>> {
        self.recorder.as_ref()
    }

    /// Whether a sink is installed.
    pub(crate) fn active(&self) -> bool {
        self.recorder.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> std::sync::Arc<std::sync::Mutex<crate::recording::AsciicastRecorder>> {
        std::sync::Arc::new(std::sync::Mutex::new(
            crate::recording::AsciicastRecorder::new(80, 24, false),
        ))
    }

    #[test]
    fn starts_inactive() {
        let st = RecordingState::new();
        assert!(!st.active());
        assert!(st.sink().is_none());
    }

    #[test]
    fn install_then_take_hands_back_the_sink() {
        let mut st = RecordingState::new();
        let s = sink();
        st.install(s.clone(), true);
        assert!(st.active());
        assert!(std::sync::Arc::ptr_eq(st.sink().unwrap(), &s));
        let taken = st.take();
        assert!(taken.is_some());
        assert!(std::sync::Arc::ptr_eq(&taken.unwrap(), &s));
        assert!(!st.active(), "take clears the holder");
    }

    #[test]
    fn suppression_gates_are_safe_without_a_sink() {
        // The whole point of the Option-shape mirrors: suppression on an
        // inactive recording must be a no-op, never a panic.
        let st = RecordingState::new();
        st.suppress_all();
        st.resume_all();
        st.suppress_input_only();
        st.resume_input_only();
    }
}
