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
                });
                steps
            },
            ..self.scenario.clone()
        };
    }

    /// Build the recorded scenario.
    pub fn build(self) -> Scenario {
        self.scenario
    }
}
