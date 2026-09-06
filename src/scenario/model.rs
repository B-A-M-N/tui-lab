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
    /// Declared scenario parameters (re-review P0.3). A recorded secret
    /// never lands in the scenario file — the recorder emits a
    /// `${PARAMETER}` reference and declares the parameter here. Replay
    /// resolves references from caller-supplied values; an unresolvable
    /// reference fails the step as `unresolved_parameter`, never as a
    /// parse error masquerading as a corrupt scenario.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<SensitiveParameter>,
    /// What happens when a step fails (audit finding 5). `Stop` (the
    /// default) halts the replay at the first failure and marks every
    /// remaining step `skipped_due_to_prior_failure` — continuing to send
    /// input after a step failed compounds whatever went wrong (a missed
    /// modal means every later keystroke lands somewhere unintended).
    /// `Continue` restores run-to-completion semantics for callers that
    /// explicitly want a full pass/fail census.
    #[serde(default)]
    pub on_failure: FailurePolicy,
    /// Ordered steps to execute.
    pub steps: Vec<ScenarioStep>,
}

/// Replay failure policy (audit finding 5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// Halt at the first failed step; later steps are skipped and counted.
    #[default]
    Stop,
    /// Run every step regardless of failures.
    Continue,
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
    /// The kind of step: "act", "wait", "assert", or "intent" (P0-9).
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
    /// Beta-audit P0-9: a recorded semantic intent. The step carries the
    /// INTENT (target selector + verb), not raw keys — replay re-resolves
    /// the target against the live screen and runs the same focus-secured
    /// plan engine, so a recording survives layout/focus changes that
    /// would break a replayed key sequence.
    Intent,
}

/// One declared scenario parameter (re-review P0.3): a named, typed slot the
/// scenario's steps may reference as `${NAME}`. The VALUE never lives in the
/// scenario — only the fact that a caller must supply one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensitiveParameter {
    /// Reference name, spelled `${name}` inside step payloads.
    pub name: String,
    /// What kind of secret it is (advisory: names the input masking the
    /// caller should expect, e.g. a password vs a token).
    #[serde(default)]
    pub kind: SensitiveKind,
    /// Human-facing description of what to supply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The kind of a scenario parameter (advisory metadata).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveKind {
    /// A password or passphrase.
    #[default]
    Password,
    /// An API/token credential.
    Token,
    /// Any other secret text.
    Secret,
}

impl SensitiveParameter {
    /// The `${NAME}` reference spelling for this parameter.
    pub fn reference(&self) -> String {
        format!("${{{}}}", self.name)
    }
}

/// A caller-supplied parameter value for replay.
#[derive(Debug, Clone)]
pub struct ParameterValue {
    pub name: String,
    pub value: String,
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
            parameters: Vec::new(),
            on_failure: FailurePolicy::default(),
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

    /// Add an intent step (beta-audit P0-9): target + verb recorded as
    /// semantic facts; replay re-resolves and focus-secures.
    pub fn intent(mut self, params: serde_json::Value) -> Self {
        self.steps.push(ScenarioStep {
            kind: StepKind::Intent,
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

    /// The declared parameter names (re-review P0.3), so a caller can learn
    /// what it must supply before replay.
    pub fn parameter_names(&self) -> Vec<&str> {
        self.parameters.iter().map(|p| p.name.as_str()).collect()
    }

    /// Substitute `${NAME}` references throughout every step's params using
    /// `values`. Unresolved references are LEFT AS-IS: resolution failure is
    /// reported per-step at replay time (`unresolved_parameter`), not as a
    /// scenario-wide parse error — and a caller that resolves nothing still
    /// gets an honest run with failures naming the missing parameters.
    pub fn resolve_parameters(&self, values: &[ParameterValue]) -> Vec<serde_json::Value> {
        self.steps
            .iter()
            .map(|step| substitute(step.params.clone(), self, values))
            .collect()
    }
}

/// Walk a step's JSON, replacing `${NAME}` strings for which a value was
/// supplied. Values are matched whole-string (a `"${PASSWORD}"` payload)
/// and embedded (`"user:${USER}"`) — but never let a value inject JSON
/// structure: substitution happens at the string level only.
fn substitute(
    mut v: serde_json::Value,
    scenario: &Scenario,
    values: &[ParameterValue],
) -> serde_json::Value {
    match &mut v {
        serde_json::Value::String(s) => {
            for p in &scenario.parameters {
                let reference = p.reference();
                if s.contains(&reference) {
                    if let Some(val) = values.iter().find(|v| v.name == p.name) {
                        *s = s.replace(&reference, &val.value);
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                *item = substitute(item.take(), scenario, values);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                *item = substitute(item.take(), scenario, values);
            }
        }
        _ => {}
    }
    v
}
