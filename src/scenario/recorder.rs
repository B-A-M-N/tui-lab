//! Scenario recorder (spec item 39).
//!
//! Records act/wait/assert steps as they happen, producing a Scenario object.
//! The recorded scenario can be saved, replayed, or compared.

use super::model::Scenario;

/// Records steps as they are performed.
pub struct ScenarioRecorder {
    scenario: Scenario,
}

impl ScenarioRecorder {
    /// Create a new recorder for a scenario with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        ScenarioRecorder {
            scenario: Scenario::new(name),
        }
    }

    /// Record an act step.
    pub fn record_act(&mut self, params: serde_json::Value) {
        self.scenario = Scenario {
            steps: {
                let mut steps = self.scenario.steps.clone();
                steps.push(super::model::ScenarioStep {
                    kind: super::model::StepKind::Act,
                    params,
                    expect: None,
                });
                steps
            },
            ..self.scenario.clone()
        };
    }

    /// Record an act step WITH its mutation guard (re-review Wave-2): the
    /// captured before-frame's structure hash and resolved focus become the
    /// `expect` precondition the runner verifies before replaying this step.
    /// Recording guards is opt-in per step; replaying an unguarded step is
    /// the historical, always-allowed path.
    pub fn record_act_with_expect(
        &mut self,
        params: serde_json::Value,
        tx: &crate::execution::InteractionTransaction,
    ) {
        let expect = super::model::StepExpect {
            structure_hash: Some(tx.before_frame.state.structure_hash.clone()),
            focus_control_id: tx.focus_before.as_ref().and_then(|f| f.0.clone()),
            text_present: None,
        };
        self.scenario = Scenario {
            steps: {
                let mut steps = self.scenario.steps.clone();
                steps.push(super::model::ScenarioStep {
                    kind: super::model::StepKind::Act,
                    params,
                    expect: Some(expect),
                });
                steps
            },
            ..self.scenario.clone()
        };
    }

    /// Record a SENSITIVE act step (re-review P0.3): the payload is stripped
    /// to a `${NAME}` reference and the parameter is declared on the
    /// scenario. The scenario file carries the shape of the step and the
    /// fact that a caller must supply a value — never the value itself.
    /// Replay without a supplied value fails the step as
    /// `unresolved_parameter` (a structured, honest outcome), not as a
    /// corrupt/unparseable scenario.
    ///
    /// The parameter name is derived here (`{FIELD}_{n}`, one slot per
    /// recorded step) so multiple sensitive steps never share a slot.
    pub fn record_act_sensitive(
        &mut self,
        mut action_params: serde_json::Value,
        payload_field: &str,
        kind: super::model::SensitiveKind,
        byte_len: usize,
    ) {
        use serde_json::Value;
        let param_name = format!(
            "{}_{}",
            payload_field.to_uppercase(),
            self.scenario.steps.len() + 1
        );
        // Sanity: the payload field must be a string for reference
        // substitution to work (type/paste payloads are strings).
        if let Some(payload) = action_params.get(payload_field) {
            debug_assert!(payload.is_string(), "sensitive payload must be a string");
        }
        debug_assert!(
            !self
                .scenario
                .parameters
                .iter()
                .any(|p| p.name == param_name),
            "parameter names are one-per-step; collision means bookkeeping drift"
        );
        self.scenario
            .parameters
            .push(super::model::SensitiveParameter {
                name: param_name.clone(),
                kind,
                description: Some(format!(
                    "sensitive payload recorded as redacted ({byte_len} bytes); \
                     supply the value to replay this step"
                )),
            });
        let reference = Value::String(format!("${{{param_name}}}"));
        if let Value::Object(map) = &mut action_params {
            map.insert(payload_field.to_string(), reference);
            // Marker fields make the redaction inspectable in the file.
            map.insert("sensitive".to_string(), Value::Bool(true));
            map.insert("payload_bytes".to_string(), Value::from(byte_len as u64));
        }
        self.record_act(action_params);
    }

    /// Record a wait step.
    pub fn record_wait(&mut self, params: serde_json::Value) {
        self.scenario = Scenario {
            steps: {
                let mut steps = self.scenario.steps.clone();
                steps.push(super::model::ScenarioStep {
                    kind: super::model::StepKind::Wait,
                    params,
                    expect: None,
                });
                steps
            },
            ..self.scenario.clone()
        };
    }

    /// Record an assert step.
    pub fn record_assert(&mut self, params: serde_json::Value) {
        self.scenario = Scenario {
            steps: {
                let mut steps = self.scenario.steps.clone();
                steps.push(super::model::ScenarioStep {
                    kind: super::model::StepKind::Assert,
                    params,
                    expect: None,
                });
                steps
            },
            ..self.scenario.clone()
        };
    }

    /// Number of steps recorded so far (live progress reporting).
    pub fn step_count_hint(&self) -> usize {
        self.scenario.steps.len()
    }

    /// Build the recorded scenario.
    pub fn build(self) -> Scenario {
        self.scenario
    }
}
