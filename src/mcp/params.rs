//! Tool parameter structs (schemas) for the MCP tools.
//!
//! Item 30: tui_act uses a tagged enum (`TuiActRequest`) so Hermes gets a
//! proper discriminated union schema instead of a flat struct with many
//! optional fields that permits nonsensical combinations.
//!
//! Wave G item 70: every action/mode/condition/assertion/profile selector is
//! a typed enum, not a `String` matched deep in a handler. Each enum pairs a
//! closed variant list (which generates a real JSON Schema `enum`) with the
//! [`Known::Other`] escape hatch: an *unknown* value still deserializes (so
//! the handler can answer `invalid_request` with the full expected list
//! through the normal envelope) instead of dying in the transport layer with
//! a bare JSON-RPC deserialization error that carries no remediation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A selector value that is either one of the known variants or an
/// unrecognized string. `Known` gives typed dispatch + closed schemas;
/// `Other` keeps forward compatibility honest — new/typo'd values surface as
/// envelope `invalid_request` naming every accepted value, never as a
/// transport-level parse failure with no context.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Known<T> {
    Known(T),
    Other(String),
}

impl<T> Known<T> {
    /// The known variant, or `None` when the caller passed an unrecognized
    /// value.
    pub fn known(&self) -> Option<&T> {
        match self {
            Known::Known(t) => Some(t),
            Known::Other(_) => None,
        }
    }
}

/// `From<&str>` for test/authoring ergonomics: a recognized wire name
/// becomes `Known`, anything else becomes `Known::Other` (which the
/// handlers surface as `invalid_request` naming the accepted set). The MCP
/// transport path never uses this — serde's untagged derive handles the
/// wire — but test constructors and scenario authors write `"text".into()`.
impl<T: std::str::FromStr<Err = String>> From<&str> for Known<T> {
    fn from(s: &str) -> Self {
        match T::from_str(s) {
            Ok(t) => Known::Known(t),
            Err(_) => Known::Other(s.to_string()),
        }
    }
}

/// Trait for the closed selector enums: their variant names, for error
/// messages and the capability registry. Hand-rolled (no strum dependency).
pub trait EnumVariants {
    const VARIANTS: &'static [&'static str];
}

macro_rules! selector_enum {
    (
        $(#[$meta:meta])*
        $name:ident ; [ $( $variant:ident => $wire:literal ),+ $(,)? ]
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
        pub enum $name {
            $(
                #[serde(rename = $wire)]
                $variant,
            )+
        }

        impl EnumVariants for $name {
            const VARIANTS: &'static [&'static str] = &[ $( $wire ),+ ];
        }

        impl $name {
            /// Wire name of this variant (the exact historical string the
            /// handler matched before the enum existed).
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $wire, )+
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $wire => Ok($name::$variant), )+
                    other => Err(format!(
                        "unknown {} '{}' (expected one of: {})",
                        stringify!($name), other,
                        Self::VARIANTS.join(", ")
                    )),
                }
            }
        }
    };
}

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

// ── Wave G item 70: the closed selector vocabularies ─────────────────────
// One enum per stringly selector. The wire names are the exact historical
// strings the handlers matched before, so existing callers keep working.

selector_enum!(
    /// `tui_session` action.
    SessionAction;
    [
        Start => "start", Restart => "restart", Stop => "stop", List => "list",
        Status => "status", Lease => "lease", Release => "release",
    ]
);

selector_enum!(
    /// `tui_observe` mode.
    ObserveMode;
    [
        Summary => "summary", Screen => "screen", Cells => "cells",
        Semantic => "semantic", Tree => "tree", Nodes => "nodes",
        Diff => "diff", Changes => "changes", Scrollback => "scrollback",
        Search => "search", CommandState => "command_state", History => "history",
    ]
);

selector_enum!(
    /// `tui_wait` condition (the `build_wait` vocabulary).
    WaitCondition;
    [
        Text => "text", TextAbsent => "text_absent", ScreenChange => "screen_change",
        ScreenStable => "screen_stable", ProcessExit => "process_exit",
        Title => "title", Bell => "bell", Idle => "idle",
        CommandDone => "command_done", CommandOutput => "command_output",
    ]
);

selector_enum!(
    /// `tui_assert` assertion (includes `oracle`, the Wave E shared
    /// language entry point).
    AssertAssertion;
    [
        Text => "text", TextAbsent => "text_absent", Position => "position",
        Focus => "focus", NotClipped => "not_clipped", Dimensions => "dimensions",
        ExitCode => "exit_code", Region => "region", Snapshot => "snapshot",
        Structure => "structure", ControlExists => "control_exists",
        FocusedNot => "focused_not", Oracle => "oracle",
    ]
);

selector_enum!(
    /// `tui_checkpoint` action.
    CheckpointAction;
    [ Save => "save", Compare => "compare", List => "list", Delete => "delete" ]
);

selector_enum!(
    /// `tui_scenario` action.
    ScenarioAction;
    [
        List => "list", RecordStart => "record_start", RecordStop => "record_stop",
        Save => "save", Export => "export", Run => "run",
    ]
);

selector_enum!(
    /// `tui_record` format / lifecycle selector.
    RecordFormat;
    [ Start => "start", Stop => "stop", Cast => "cast", Svg => "svg", Png => "png" ]
);

selector_enum!(
    /// `tui_explore` mode.
    ExploreMode;
    [
        Random => "random", GuidedCandidates => "guided_candidates",
        Semantic => "semantic", StateGraph => "state_graph",
    ]
);

selector_enum!(
    /// `tui_audit` profile. `layout` is a documented alias of `resize`;
    /// `contract` (Wave E) folds conformance into findings.
    AuditProfile;
    [
        Full => "full", Keyboard => "keyboard", Focus => "focus", Resize => "resize",
        Layout => "layout", Clipping => "clipping", Discoverability => "discoverability",
        Navigation => "navigation", Contract => "contract", Color => "color",
        Performance => "performance", Mouse => "mouse", States => "states",
        Errors => "errors",
    ]
);

selector_enum!(
    /// `tui_coverage` action.
    CoverageAction;
    [
        Detect => "detect", Summary => "summary", Collect => "collect",
        Delta => "delta", Uncovered => "uncovered", Ledger => "ledger",
        Start => "start", Stop => "stop",
    ]
);

selector_enum!(
    /// `tui_framework` action.
    FrameworkAction;
    [ Detect => "detect", Capabilities => "capabilities", AdapterSnippet => "adapter_snippet" ]
);

selector_enum!(
    /// `tui_run` action.
    RunAction;
    [ Status => "status", Persist => "persist", Close => "close", Context => "context" ]
);

selector_enum!(
    /// `tui_contract` action.
    ContractAction;
    [ Load => "load", Validate => "validate", Status => "status", Compare => "compare" ]
);

impl AuditProfile {
    /// The engine-level name (what `crate::audit::orchestrator` accepts).
    /// `layout` maps to `resize` at the engine, but the wire name is kept
    /// for the response's `profile` echo.
    pub fn engine_name(&self) -> &'static str {
        match self {
            AuditProfile::Layout => "resize",
            other => other.as_str(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiSessionParams {
    pub action: Known<SessionAction>,
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
    /// Isolation profile (Wave G item 77): `local` | `clean` | `strict`.
    /// Typed as Known<IsolationParam> so an unknown name still reaches the
    /// envelope as invalid_request with the accepted list.
    #[serde(default)]
    pub isolation: Option<Known<IsolationParam>>,
    #[serde(default)]
    pub id: Option<String>,
    /// lease: who takes control (free-form label).
    #[serde(default)]
    pub holder: Option<String>,
    /// lease: time-to-live in milliseconds (default 300000 = 5 min).
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

/// Isolation profile vocabulary (Wave G item 77).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum IsolationParam {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "clean")]
    Clean,
    #[serde(rename = "strict")]
    Strict,
}

impl EnumVariants for IsolationParam {
    const VARIANTS: &'static [&'static str] = &["local", "clean", "strict"];
}

impl IsolationParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            IsolationParam::Local => "local",
            IsolationParam::Clean => "clean",
            IsolationParam::Strict => "strict",
        }
    }
}

impl From<IsolationParam> for crate::session::isolation::Isolation {
    fn from(p: IsolationParam) -> Self {
        match p {
            IsolationParam::Local => crate::session::isolation::Isolation::Local,
            IsolationParam::Clean => crate::session::isolation::Isolation::Clean,
            IsolationParam::Strict => crate::session::isolation::Isolation::Strict,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiObserveParams {
    #[serde(default)]
    pub mode: Option<Known<ObserveMode>>,
    #[serde(default)]
    pub idle_ms: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
    /// mode=changes: which consumer cursor to read/advance (Wave B item 13).
    /// Distinct consumers ("hermes", "audit", "explorer", ...) each keep
    /// their own position in the event stream.
    #[serde(default)]
    pub consumer: Option<String>,
    /// mode=search: the query string (Wave F item 53). `text` is accepted as
    /// an alias.
    #[serde(default)]
    pub query: Option<String>,
    /// Alias for `query` (mode=search).
    #[serde(default)]
    pub text: Option<String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiWaitParams {
    pub condition: Known<WaitCondition>,
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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiAssertParams {
    pub assertion: Known<AssertAssertion>,
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

impl TuiAssertParams {
    /// The assertion wire name, or the raw unknown string for the
    /// `invalid_request` message.
    pub fn assertion_name(&self) -> &str {
        match &self.assertion {
            Known::Known(a) => a.as_str(),
            Known::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiCheckpointParams {
    pub action: Known<CheckpointAction>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiScenarioParams {
    pub action: Known<ScenarioAction>,
    #[serde(default)]
    pub name: Option<String>,
    /// Target session (record_start resolves id + generation from it).
    #[serde(default)]
    pub id: Option<String>,
    /// Opaque recording identity from record_start (preferred over name for
    /// record_stop; names are display labels, not identities).
    #[serde(default)]
    pub recording_id: Option<String>,
    #[serde(default)]
    pub steps: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRecordParams {
    #[serde(default)]
    pub format: Option<Known<RecordFormat>>,
    #[serde(default)]
    pub id: Option<String>,
}

/// Run lifecycle (goal spec): status / persist / close / context. Ephemeral
/// by default; `persist` promotes the SAME run to durable storage.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRunParams {
    pub action: Known<RunAction>,
    /// persist: explicit artifact root. When omitted, resolved from the
    /// primary session's `LaunchSpec.cwd` — never this process's cwd.
    #[serde(default)]
    pub root: Option<String>,
    /// close: kill sessions too? (default false — close never touches
    /// sessions unless explicitly told to).
    #[serde(default)]
    pub kill_sessions: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiExploreParams {
    pub mode: Known<ExploreMode>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub actions: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub recording_path: Option<String>,
    /// guided_candidates: highest risk class the caller accepts
    /// (`safe` < `mutating` < `destructive` < `external_side_effect`).
    /// Candidates above the allowance are filtered, never merely flagged
    /// (Wave D item 33).
    #[serde(default)]
    pub max_risk: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiAuditParams {
    #[serde(default)]
    pub profile: Option<Known<AuditProfile>>,
    #[serde(default)]
    pub id: Option<String>,
    /// Wave G item 67: record the result under this label, then diff against
    /// the `compare_to` baseline (FIXED/REGRESSED/NEW per finding).
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub compare_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiCoverageParams {
    #[serde(default)]
    pub action: Option<Known<CoverageAction>>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiFrameworkParams {
    pub action: Known<FrameworkAction>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Wave E items 45–47: contract loading, validation, conformance status,
/// and comparison against a saved baseline.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiContractParams {
    /// load | validate | status | compare
    pub action: Known<ContractAction>,
    /// Path to the contract document (YAML or JSON).
    #[serde(default)]
    pub path: Option<String>,
    /// Target session (defaults to the active one).
    #[serde(default)]
    pub id: Option<String>,
    /// compare: baseline label to compare against (defaults to "baseline").
    #[serde(default)]
    pub baseline: Option<String>,
    /// compare: label for the current run being compared (defaults to "current").
    #[serde(default)]
    pub label: Option<String>,
}
