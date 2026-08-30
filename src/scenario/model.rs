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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioStep {
    /// The kind of step: "act", "wait", or "assert".
    pub kind: StepKind,
    /// The action or assertion parameters.
    #[serde(flatten)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Act,
    Wait,
    Assert,
}

impl Scenario {
    /// Create a new scenario with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Scenario {
            schema: "tui-lab/scenario/v1".to_string(),
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
        });
        self
    }

    /// Add a wait step.
    pub fn wait(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Wait,
            params,
        });
        self
    }

    /// Add an assert step.
    pub fn assert(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Assert,
            params,
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
