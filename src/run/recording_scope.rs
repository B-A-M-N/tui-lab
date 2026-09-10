//! Session-scoped scenario recordings (audit re-review item 5).
//!
//! A recording is identified by its [`ScenarioRecordingId`], not its
//! human-readable name — two sessions can legitimately record scenarios both
//! called "login". Each recording is bound to one `session_id + generation`,
//! and only tool calls resolving to that exact session generation are
//! absorbed. A recorder for session A must never capture activity sent to
//! session B.

use crate::scenario::recorder::ScenarioRecorder;

/// Opaque recording identity (`rec-<uuid simple>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScenarioRecordingId(String);

impl ScenarioRecordingId {
    pub fn generate() -> Self {
        ScenarioRecordingId(format!("rec-{}", uuid::Uuid::new_v4().simple()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScenarioRecordingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One in-progress scenario recording, scoped to a session generation.
pub struct ScenarioRecording {
    pub id: ScenarioRecordingId,
    /// Human-readable name (display + default scenario name; NOT an identity).
    pub name: String,
    /// Session whose traffic this recording captures.
    pub session_id: String,
    /// Exact session generation — after a restart the generation changes and
    /// a stale recording must NOT keep absorbing the relaunched session's
    /// traffic (it observes a different process).
    pub generation: u32,
    pub started_at: u64,
    pub recorder: ScenarioRecorder,
}

impl ScenarioRecording {
    pub fn new(name: impl Into<String>, session_id: impl Into<String>, generation: u32) -> Self {
        let name = name.into();
        ScenarioRecording {
            id: ScenarioRecordingId::generate(),
            recorder: ScenarioRecorder::new(name.clone()),
            name,
            session_id: session_id.into(),
            generation,
            started_at: now_ms(),
        }
    }

    /// Whether a tool call resolved to this recording's exact session
    /// generation. Only then may steps be appended.
    pub fn matches(&self, session_id: &str, generation: u32) -> bool {
        self.session_id == session_id && self.generation == generation
    }

    pub fn record_act(&mut self, params: serde_json::Value) {
        self.recorder.record_act(params);
    }
    /// Record a first-class intent step (beta-audit P0-9): target + verb
    /// as semantic facts; replay re-resolves the target and re-runs the
    /// focus-secured plan.
    pub fn record_intent(&mut self, params: serde_json::Value) {
        self.recorder.record_intent(params);
    }
    /// Record a sensitive act step (re-review P0.3): the payload field is
    /// replaced with a `${NAME}` reference and the parameter is declared on
    /// the scenario — the value itself never lands in the file.
    pub fn record_act_sensitive(
        &mut self,
        action_params: serde_json::Value,
        payload_field: &str,
        kind: crate::scenario::model::SensitiveKind,
        byte_len: usize,
    ) {
        self.recorder
            .record_act_sensitive(action_params, payload_field, kind, byte_len);
    }
    /// Record an act step with a replay precondition derived by the
    /// driving pipeline (ordinary live recording).
    pub fn record_act_with_expect(
        &mut self,
        params: serde_json::Value,
        expect: crate::scenario::model::StepExpect,
    ) {
        self.recorder.record_act_with_expect(params, &expect);
    }
    pub fn record_wait(&mut self, params: serde_json::Value) {
        self.recorder.record_wait(params);
    }
    pub fn record_assert(&mut self, params: serde_json::Value) {
        self.recorder.record_assert(params);
    }

    /// Finish: detach the recorder and return the completed scenario.
    pub fn finish(&mut self) -> crate::scenario::model::Scenario {
        let rec = std::mem::replace(&mut self.recorder, ScenarioRecorder::new(&self.name));
        rec.build()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_matches_only_its_session_generation() {
        let rec = ScenarioRecording::new("login", "sess-A", 0);
        assert!(rec.matches("sess-A", 0));
        assert!(!rec.matches("sess-B", 0), "other session must not match");
        assert!(
            !rec.matches("sess-A", 1),
            "same session next generation must not match"
        );
    }

    #[test]
    fn recording_ids_are_unique_for_same_name() {
        let a = ScenarioRecording::new("login", "sess-A", 0);
        let b = ScenarioRecording::new("login", "sess-B", 0);
        assert_ne!(a.id, b.id, "names are not identities");
    }

    #[test]
    fn finish_returns_recorded_steps_and_detaches() {
        let mut rec = ScenarioRecording::new("flow", "s", 0);
        rec.record_act(serde_json::json!({"action": "key", "key": "tab"}));
        rec.record_assert(serde_json::json!({"assertion": "text", "text": "x"}));
        let scenario = rec.finish();
        assert_eq!(scenario.step_count(), 2);
        // Recording after finish() starts a fresh (empty) buffer, so a stale
        // append cannot leak into an already-completed scenario.
        let scenario2 = rec.finish();
        assert_eq!(scenario2.step_count(), 0);
    }
}
