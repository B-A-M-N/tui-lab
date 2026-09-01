//! Scenario model (spec item 39).
//!
//! A scenario is a sequence of steps: act, wait, assert.
//! Each step has a kind and parameters.

use serde::{Deserialize, Serialize};

/// Top-level scenario structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Schema version for forward compatibility.
    pub schema: String,
    /// Stable identity (re-review Wave-1 item 9). The name is display/search
    /// metadata; the id is the storage key, so two recordings that happen to
    /// share a name (e.g. "login" recorded in two sessions) never collide.
    /// Defaults on deserialize so scenario files saved before ids existed
    /// still load.
    #[serde(default = "Scenario::generate_id")]
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Optional metadata.
    pub metadata: Option<ScenarioMetadata>,
    /// Whether to inherit the current session or start fresh.
    #[serde(default = "default_inherit_session")]
    pub inherit_session: bool,
    /// Optional launch spec override (when inherit_session is false).
    pub launch: Option<ScenarioLaunch>,
    /// Ordered steps to execute.
    pub steps: Vec<ScenarioStep>,
}

fn default_inherit_session() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMetadata {
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub created_at: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioLaunch {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// A single step in a scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioStep {
    /// The kind of step: "act", "wait", or "assert".
    pub kind: StepKind,
    /// The action or assertion parameters.
    #[serde(flatten)]
    pub params: serde_json::Value,
    /// Mutation guard (re-review Wave-2): the state the step was captured
    /// against, verified before the step executes on replay. `None` (the
    /// historical shape — and the default on deserialize for old scenario
    /// files) means no guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<StepExpect>,
}

/// The recorded preconditions for one step, checked before the step's input
/// lands. If the app has drifted — a modal opened, the list reordered, focus
/// moved — the step FAILS with `stale_state` instead of sending a keystroke
/// into the wrong UI and corrupting both the app and the replay verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepExpect {
    /// Structure hash observed at capture time (layout skeleton, normalized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure_hash: Option<String>,
    /// Focused control id at capture time, when one was resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_control_id: Option<String>,
    /// Text the step required to be present at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_present: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Act,
    Wait,
    Assert,
}

impl Scenario {
    /// Generate a fresh scenario id (`scn-<uuid>`).
    pub fn generate_id() -> String {
        format!("scn-{}", uuid::Uuid::new_v4().simple())
    }

    /// Create a new scenario with the given name (a fresh id is assigned).
    pub fn new(name: impl Into<String>) -> Self {
        Scenario {
            schema: "tui-lab/scenario/v1".to_string(),
            id: Self::generate_id(),
            name: name.into(),
            metadata: None,
            inherit_session: true,
            launch: None,
            steps: Vec::new(),
        }
    }

    /// Add an act step.
    pub fn act(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Act,
            params,
            expect: None,
        });
        self
    }

    /// Add a wait step.
    pub fn wait(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Wait,
            params,
            expect: None,
        });
        self
    }

    /// Add an assert step.
    pub fn assert(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Assert,
            params,
            expect: None,
        });
        self
    }

    /// Get the total number of steps.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Validate that the scenario has at least one step.
    pub fn is_valid(&self) -> bool {
        !self.steps.is_empty()
    }
}
