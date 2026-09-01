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
