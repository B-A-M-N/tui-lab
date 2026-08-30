//! Tool parameter structs (schemas) for the 12 MCP tools.
//!
//! Item 30: tui_act uses a tagged enum (`TuiActRequest`) so Hermes gets a
//! proper discriminated union schema instead of a flat struct with many
//! optional fields that permits nonsensical combinations.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Typed mouse button (audit item 5/28): validated at the deserialization
/// boundary so an invalid button is `invalid_request`, never silently coerced
/// to `left` deep in the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MouseButtonParam {
    Left,
    Middle,
    Right,
}

impl From<MouseButtonParam> for crate::backend::MouseButton {
    fn from(b: MouseButtonParam) -> Self {
        match b {
            MouseButtonParam::Left => crate::backend::MouseButton::Left,
            MouseButtonParam::Middle => crate::backend::MouseButton::Middle,
            MouseButtonParam::Right => crate::backend::MouseButton::Right,
        }
    }
}

/// Typed scroll direction. Absent means `down` (documented default); an
/// unknown string is rejected by serde rather than coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirectionParam {
    Up,
    Down,
}

impl From<ScrollDirectionParam> for crate::backend::ScrollDirection {
    fn from(d: ScrollDirectionParam) -> Self {
        match d {
            ScrollDirectionParam::Up => crate::backend::ScrollDirection::Up,
            ScrollDirectionParam::Down => crate::backend::ScrollDirection::Down,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiSessionParams {
    pub action: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub isolation: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiObserveParams {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub idle_ms: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
}

/// Schema wrapper for [`TuiActRequest`]: re-roots the enum's natural `oneOf`
/// schema as `{"type":"object","oneOf":[...]}` so the MCP inputSchema passes
/// rmcp's root-object validation. The variant list is generated from a
/// *mirror* enum that derives the same serde/schemars attributes, so the
/// custom `schema_with` is not re-entered (that would recurse forever).
///
/// Serde deserialization of `TuiActRequest` is untouched.
#[doc(hidden)]
pub struct TuiActRequestSchema;

impl TuiActRequestSchema {
    pub fn wrapped(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        use schemars::json_schema;
        let variants = <TuiActRequestVariants as schemars::JsonSchema>::json_schema(gen);
        json_schema!({
            "type": "object",
            "oneOf": variants,
        })
    }
}

/// Mirror of [`TuiActRequest`] for schema generation only. Keep field-for-field
/// identical; `doc(hidden)` so it never appears in the public API story.
#[doc(hidden)]
#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TuiActRequestVariants {
    Key {
        key: String,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    Keys {
        keys: Vec<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    Type {
        text: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    Paste {
        paste: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    Raw {
        raw: Vec<u8>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MouseClick {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MousePress {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MouseRelease {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MouseMove {
        x: u16,
        y: u16,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MouseDrag {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    MouseScroll {
        x: u16,
        y: u16,
        #[serde(default)]
        direction: Option<ScrollDirectionParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        id: Option<String>,
    },
    Signal {
        signal: i32,
        #[serde(default)]
        id: Option<String>,
    },
}

/// Tagged enum for `tui_act` requests (spec item 30).
///
/// Each variant carries exactly the fields needed for that action — no more,
/// no less. This gives Hermes a meaningful schema instead of one giant struct
/// where any field can appear with any action.
///
/// The MCP inputSchema root is wrapped as `type: object` (see
/// `TuiActRequestSchema`) because rmcp 3.1.4 requires root `type: object`
/// (MCP spec); serde's internally-tagged enum alone generates a bare `oneOf`
/// there, which panicked the tool router on every stdio `tools/list` /
/// `tools/call`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
#[schemars(schema_with = "TuiActRequestSchema::wrapped")]
pub enum TuiActRequest {
    /// Send a single key event (e.g. "enter", "ctrl+c", "tab").
    Key {
        key: String,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Send a sequence of key events.
    Keys {
        keys: Vec<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Type text into the terminal.
    Type {
        text: String,
        /// Audit item 28: mark this payload as sensitive so downstream
        /// recording/logging hooks can redact it.
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Paste text (with bracketed paste escape if negotiated).
    Paste {
        paste: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Send raw bytes.
    Raw {
        raw: Vec<u8>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse click (press + release).
    MouseClick {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse press only.
    MousePress {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse release only.
    MouseRelease {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse move (no button).
    MouseMove {
        x: u16,
        y: u16,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse drag.
    MouseDrag {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Mouse scroll.
    MouseScroll {
        x: u16,
        y: u16,
        #[serde(default)]
        direction: Option<ScrollDirectionParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Resize the terminal.
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        id: Option<String>,
    },
    /// Send a signal to the child process group.
    Signal {
        signal: i32,
        #[serde(default)]
        id: Option<String>,
    },
}

/// Helper to extract common fields from any act variant.
impl TuiActRequest {
    pub fn no_wait(&self) -> bool {
        match self {
            TuiActRequest::Key { no_wait, .. } => *no_wait,
            TuiActRequest::Keys { no_wait, .. } => *no_wait,
            TuiActRequest::Type { no_wait, .. } => *no_wait,
            TuiActRequest::Paste { no_wait, .. } => *no_wait,
            TuiActRequest::Raw { no_wait, .. } => *no_wait,
            TuiActRequest::MouseClick { no_wait, .. } => *no_wait,
            TuiActRequest::MousePress { no_wait, .. } => *no_wait,
            TuiActRequest::MouseRelease { no_wait, .. } => *no_wait,
            TuiActRequest::MouseMove { no_wait, .. } => *no_wait,
            TuiActRequest::MouseDrag { no_wait, .. } => *no_wait,
            TuiActRequest::MouseScroll { no_wait, .. } => *no_wait,
            TuiActRequest::Resize { .. } => Some(false),
            TuiActRequest::Signal { .. } => Some(false),
        }
        .unwrap_or(false)
    }

    pub fn wait_ms(&self) -> Option<u64> {
        match self {
            TuiActRequest::Key { wait_ms, .. } => *wait_ms,
            TuiActRequest::Keys { wait_ms, .. } => *wait_ms,
            TuiActRequest::Type { wait_ms, .. } => *wait_ms,
            TuiActRequest::Paste { wait_ms, .. } => *wait_ms,
            TuiActRequest::Raw { wait_ms, .. } => *wait_ms,
            TuiActRequest::MouseClick { wait_ms, .. } => *wait_ms,
            TuiActRequest::MousePress { wait_ms, .. } => *wait_ms,
            TuiActRequest::MouseRelease { wait_ms, .. } => *wait_ms,
            TuiActRequest::MouseMove { wait_ms, .. } => *wait_ms,
            TuiActRequest::MouseDrag { wait_ms, .. } => *wait_ms,
            TuiActRequest::MouseScroll { wait_ms, .. } => *wait_ms,
            TuiActRequest::Resize { .. } => None,
            TuiActRequest::Signal { .. } => None,
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            TuiActRequest::Key { id, .. } => id.as_deref(),
            TuiActRequest::Keys { id, .. } => id.as_deref(),
            TuiActRequest::Type { id, .. } => id.as_deref(),
            TuiActRequest::Paste { id, .. } => id.as_deref(),
            TuiActRequest::Raw { id, .. } => id.as_deref(),
            TuiActRequest::MouseClick { id, .. } => id.as_deref(),
            TuiActRequest::MousePress { id, .. } => id.as_deref(),
            TuiActRequest::MouseRelease { id, .. } => id.as_deref(),
            TuiActRequest::MouseMove { id, .. } => id.as_deref(),
            TuiActRequest::MouseDrag { id, .. } => id.as_deref(),
            TuiActRequest::MouseScroll { id, .. } => id.as_deref(),
            TuiActRequest::Resize { id, .. } => id.as_deref(),
            TuiActRequest::Signal { id, .. } => id.as_deref(),
        }
    }

    /// Get the action name string for this request.
    pub fn action_name(&self) -> &str {
        match self {
            TuiActRequest::Key { .. } => "key",
            TuiActRequest::Keys { .. } => "keys",
            TuiActRequest::Type { .. } => "type",
            TuiActRequest::Paste { .. } => "paste",
            TuiActRequest::Raw { .. } => "raw",
            TuiActRequest::MouseClick { .. } => "mouse_click",
            TuiActRequest::MousePress { .. } => "mouse_press",
            TuiActRequest::MouseRelease { .. } => "mouse_release",
            TuiActRequest::MouseMove { .. } => "mouse_move",
            TuiActRequest::MouseDrag { .. } => "mouse_drag",
            TuiActRequest::MouseScroll { .. } => "mouse_scroll",
            TuiActRequest::Resize { .. } => "resize",
            TuiActRequest::Signal { .. } => "signal",
        }
    }

    /// Whether this payload is marked sensitive (audit item 28). Only the
    /// text-carrying actions can be sensitive; everything else is not.
    pub fn sensitive(&self) -> bool {
        match self {
            TuiActRequest::Type { sensitive, .. } | TuiActRequest::Paste { sensitive, .. } => {
                sensitive.unwrap_or(false)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiWaitParams {
    pub condition: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub budget_ms: Option<u64>,
    /// Quiet interval for `screen_stable` / `idle` conditions (ms).
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiAssertParams {
    pub assertion: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub x: Option<u16>,
    #[serde(default)]
    pub y: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// For `exit_code`: the exact code expected (spec section 37). When omitted,
    /// the assertion only checks that the process is no longer running.
    #[serde(default)]
    pub expected_code: Option<i32>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiCheckpointParams {
    pub action: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiScenarioParams {
    pub action: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub steps: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRecordParams {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiExploreParams {
    pub mode: String,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub actions: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub recording_path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiAuditParams {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiCoverageParams {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiFrameworkParams {
    pub action: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}
