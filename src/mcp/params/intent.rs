//! tui_intent parameters: the semantic intent system (plan_intent) at the
//! MCP surface.

use super::interact::TuiCompletionParam;
use serde::{Deserialize, Serialize};

/// `tui_intent` — resolve a semantic target + verb into a focus-secured
/// execution plan BEFORE anything is sent. Default is plan-only
/// (`dry_run` semantics); `execute: true` runs the plan through the
/// canonical executor under the same lease/guard rules as `tui_act`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiIntentParams {
    /// What to act on. Objects are `{ "by": "id"|"text"|"role"|"focused", ... }`:
    /// `{"by":"id","id":"button/save"}`, `{"by":"text","text":"Save"}`,
    /// `{"by":"role","role":"button","text":"Save"}`, `{"by":"focused"}`.
    pub target: crate::intent::ActionTarget,
    /// The verb to apply. A string (`"activate"`) or
    /// `{"verb":"type","text":"..."}` for typing.
    pub verb: IntentVerbParam,
    /// Session id. Defaults to the primary session.
    #[serde(default)]
    pub id: Option<String>,
    /// Plan only (default) or plan AND execute. Planning is
    /// observational — it resolves the target and reports the exact steps
    /// and risk without sending input, so the agent can preview what will
    /// happen first. Execution re-observes FRESH before the first click
    /// (audit P0-13): the plan is re-resolved against the live frame, not
    /// the last cached one, so a UI change between planning and executing
    /// cannot land a focus click on stale geometry.
    #[serde(default)]
    pub execute: Option<bool>,
    /// Mark a `type` verb's payload sensitive (audit P0-14): the text is
    /// redacted in every artifact — run ledger, scenario recordings, the
    /// same policy `tui_act sensitive=true` applies.
    #[serde(default)]
    pub sensitive: Option<bool>,
    /// Completion override for the payload action (audit P0-15): the same
    /// spec `tui_act` takes, so semantic verbs can express `process_exit`,
    /// `may_be_silent`, exact-text oracles, or a deliberate quiet window
    /// instead of the hard-coded stable-screen default.
    #[serde(default)]
    pub completion: Option<TuiCompletionParam>,
    /// Finding 3D (two-step contract): when `execute=true`, either
    /// `plan_id` (from a prior plan-only response) or `max_risk` is
    /// honored; BOTH may be given. `plan_id` re-validates the plan's
    /// control against a fresh observation — the executed plan must land
    /// on the same control the caller previewed. `max_risk` is the
    /// explicit risk fence: a plan whose risk exceeds it is refused
    /// (`risk_fence` error) before anything is sent. Destructive,
    /// external, and unknown risks always require an explicit
    /// `max_risk` covering them — planning alone never authorizes them.
    #[serde(default)]
    pub plan_id: Option<String>,
    /// Explicit no-wait policy for the payload action. Equivalent to
    /// completion=no_wait and preserved on recorded replay.
    #[serde(default)]
    pub no_wait: Option<bool>,
    /// Explicit completion budget ceiling preserved on recorded replay.
    #[serde(default)]
    pub settle_budget_ms: Option<u64>,
    /// Finding 3D: explicit risk ceiling for execution. Accepted values:
    /// `safe` < `mutating` < `destructive` < `external_side_effect` <
    /// `unknown`. Execution is refused when the plan's risk exceeds this.
    #[serde(default)]
    pub max_risk: Option<String>,
}

/// Wire shape for the verb: a bare string for the payload-free verbs, or
/// an object carrying the text for `type`. (Kept separate from
/// [`crate::intent::ActionVerb`] because that type serializes its `type`
/// variant as `{"type":{"text":...}}` — internally tagged around the
/// payload — which is awkward to write by hand at a tool boundary.)
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum IntentVerbParam {
    /// `"activate"`, `"focus"`, `"click"`, `"toggle"`, `"select"`, `"open"`.
    Name(String),
    /// `{"verb": "type", "text": "..."}`.
    Typed { verb: String, text: String },
}

impl IntentVerbParam {
    /// Parse into the engine verb, or a caller-error naming every
    /// accepted shape.
    pub fn parse(&self) -> Result<crate::intent::ActionVerb, String> {
        use crate::intent::ActionVerb as V;
        match self {
            IntentVerbParam::Name(s) => match s.as_str() {
                "activate" => Ok(V::Activate),
                "focus" => Ok(V::Focus),
                "click" => Ok(V::Click),
                "toggle" => Ok(V::Toggle),
                "select" => Ok(V::Select),
                "open" => Ok(V::Open),
                "type" => Err("verb 'type' requires text: {\"verb\":\"type\",\"text\":\"...\"}"
                    .to_string()),
                other => Err(format!(
                    "unknown verb '{other}' (expected one of: activate, focus, click, toggle, select, open, or {{\"verb\":\"type\",\"text\":...}})"
                )),
            },
            IntentVerbParam::Typed { verb, text } => {
                if verb == "type" {
                    Ok(V::Type { text: text.clone() })
                } else {
                    Err(format!(
                        "verb '{verb}' takes no text payload — pass the bare verb name instead"
                    ))
                }
            }
        }
    }

    /// Accepted verb names, for error text.
    pub const NAMES: &'static [&'static str] = &[
        "activate", "focus", "click", "toggle", "select", "open", "type",
    ];
}
