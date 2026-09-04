//! tui_intent parameters: the semantic intent system (plan_intent) at the
//! MCP surface.

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
    /// happen first.
    #[serde(default)]
    pub execute: Option<bool>,
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
