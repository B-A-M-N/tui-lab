//! tui_act parameters (tagged request enum) and completion specs.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use serde::{Deserialize, Serialize};

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

/// Schema wrapper for [`TuiActRequest`]: re-roots the enum's natural `oneOf`
/// schema as `{"type":"object","oneOf":[...]}` so the MCP inputSchema passes
/// rmcp's root-object validation. The variant list is generated from a
/// *mirror* enum that derives the same serde/schemars attributes, so the
/// custom `schema_with` is not re-entered (that would recurse forever).
///
/// Serde deserialization of `TuiActRequest` is untouched.
#[doc(hidden)]
pub struct TuiActRequestSchema;

/// Agent-facing expected-state guard (re-review P0.9): the state the caller
/// observed when it decided to act. The executor validates it atomically
/// with the send; on drift the action is refused with a structured
/// `stale_state` error naming expected vs actual — never a misdirected
/// keystroke into a changed UI.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MutationGuardParam {
    /// Session generation at decision time; a restart invalidates the guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// Structure hash at decision time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure_hash: Option<String>,
    /// Focused control id at decision time (fused semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_control_id: Option<String>,
}

impl MutationGuardParam {
    /// Convert to the executor guard.
    pub fn to_guard(&self) -> crate::execution::MutationGuard {
        crate::execution::MutationGuard {
            generation: self.generation,
            structure_hash: self.structure_hash.clone(),
            focus_control_id: self.focus_control_id.clone(),
        }
    }
}

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

/// Agent-facing completion specification (re-review P0.1). A completion is
/// the action's declaration of what "done" means — and some completions need
/// DATA (which text to wait for, how long quiet lasts). The old flat enum
/// discarded that data: `completion: "text_appears"` deserialized into
/// `CompletionPolicy::TextAppears("")`, a policy that can never match. So
/// the wire model accepts two shapes:
///
/// * a plain string for the parameterless strategies (`"stable_screen"`,
///   `"process_exit"`, …) — backward compatible with every recorded
///   scenario and call made before this change;
/// * an object `{"type": "text_appears", "text": "Save complete"}` for the
///   parameterized ones, where the data travels inside the completion
///   itself. The completion object is self-contained: no parallel
///   `wait_text` field to keep in sync, nothing to silently drop.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TuiCompletionParam {
    /// `{"type": ..., ...}` — carries its own parameters.
    Spec(CompletionSpec),
    /// `"name"` — the parameterless shorthand, expanded losslessly.
    Name(CompletionName),
}

/// The parameterless completion strategies, usable as bare strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompletionName {
    /// Action then screen settles at rest (the ordinary default).
    StableScreen,
    /// A visible change is enough; quiet is not required.
    FirstChange,
    /// Any observable terminal activity (output, cursor, bell, title).
    AnyChange,
    /// The child process exits.
    ProcessExit,
    /// The shell command finishes (OSC 133).
    CommandDone,
    /// The terminal bell rings.
    Bell,
    /// The semantic analysis output changes.
    SemanticChange,
    /// The action may legitimately produce no observable change; never a
    /// false `settled=false` (copy to clipboard, an invisible toggle).
    MayBeSilent,
    /// Do not wait at all; act and return immediately.
    NoWait,
}

impl CompletionName {
    /// The wire name for this strategy (used in error messages).
    pub fn as_str(&self) -> &'static str {
        match self {
            CompletionName::StableScreen => "stable_screen",
            CompletionName::FirstChange => "first_change",
            CompletionName::AnyChange => "any_change",
            CompletionName::ProcessExit => "process_exit",
            CompletionName::CommandDone => "command_done",
            CompletionName::Bell => "bell",
            CompletionName::SemanticChange => "semantic_change",
            CompletionName::MayBeSilent => "may_be_silent",
            CompletionName::NoWait => "no_wait",
        }
    }

    /// The runtime [`CompletionPolicy`] this name stands for.
    pub fn to_policy(&self) -> crate::capture::CompletionPolicy {
        use crate::capture::CompletionPolicy as P;
        match self {
            CompletionName::StableScreen => P::StableScreen,
            CompletionName::FirstChange => P::FirstScreenChange,
            CompletionName::AnyChange => P::AnyObservableChange,
            CompletionName::ProcessExit => P::ProcessExit,
            CompletionName::CommandDone => P::CommandDone,
            CompletionName::Bell => P::Bell,
            CompletionName::SemanticChange => P::SemanticChange,
            CompletionName::MayBeSilent => P::MayBeSilent,
            CompletionName::NoWait => P::NoWait,
        }
    }
}

/// The self-describing completion strategies that carry their own data
/// (re-review P0.1): the text waited for travels WITH the completion, so
/// `{"type":"text_appears","text":"Saved"}` converts to
/// `CompletionPolicy::TextAppears("Saved")` — never to an empty-string
/// policy that can never match.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionSpec {
    /// Action then screen settles at rest, with a caller-chosen quiet window.
    StableScreen {
        /// Quiet-window length in ms; defaults to the executor's quiet when
        /// absent.
        #[serde(default)]
        quiet_ms: Option<u64>,
    },
    /// A named text appears — *causally*: it must not have been present
    /// before the action.
    TextAppears {
        /// The text whose appearance completes the action.
        text: String,
    },
    /// A named text disappears.
    TextDisappears {
        /// The text whose disappearance completes the action.
        text: String,
    },
}

impl CompletionSpec {
    /// The runtime [`CompletionPolicy`] this spec stands for. The text
    /// variants carry their payload through; `StableScreen` maps to the
    /// ordinary settle policy (its optional `quiet_ms` reaches the executor
    /// through [`CompletionSpec::quiet_ms`], since the runtime policy keeps
    /// quiet separate).
    pub fn to_policy(&self) -> crate::capture::CompletionPolicy {
        use crate::capture::CompletionPolicy as P;
        match self {
            CompletionSpec::StableScreen { .. } => P::StableScreen,
            CompletionSpec::TextAppears { text } => P::TextAppears(text.clone()),
            CompletionSpec::TextDisappears { text } => P::TextDisappears(text.clone()),
        }
    }

    /// The quiet-window override declared by a `stable_screen` spec, if any.
    pub fn quiet_ms(&self) -> Option<u64> {
        match self {
            CompletionSpec::StableScreen { quiet_ms } => *quiet_ms,
            _ => None,
        }
    }
}

impl TuiCompletionParam {
    /// The runtime [`CompletionPolicy`] this wire value stands for.
    pub fn to_policy(&self) -> crate::capture::CompletionPolicy {
        match self {
            TuiCompletionParam::Name(n) => n.to_policy(),
            TuiCompletionParam::Spec(s) => s.to_policy(),
        }
    }

    /// The quiet-window override declared by this wire value, if any
    /// (`{"type":"stable_screen","quiet_ms":400}`).
    pub fn quiet_ms(&self) -> Option<u64> {
        match self {
            TuiCompletionParam::Spec(s) => s.quiet_ms(),
            TuiCompletionParam::Name(_) => None,
        }
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    Keys {
        keys: Vec<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    Type {
        text: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    Paste {
        paste: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    Raw {
        raw: Vec<u8>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MouseClick {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MousePress {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MouseRelease {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MouseMove {
        x: u16,
        y: u16,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MouseDrag {
        x: u16,
        y: u16,
        #[serde(default)]
        button: Option<MouseButtonParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    MouseScroll {
        x: u16,
        y: u16,
        #[serde(default)]
        direction: Option<ScrollDirectionParam>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
    },
    Signal {
        signal: i32,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Send a sequence of key events.
    Keys {
        keys: Vec<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Paste text (with bracketed paste escape if negotiated).
    Paste {
        paste: String,
        #[serde(default)]
        sensitive: Option<bool>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Send raw bytes.
    Raw {
        raw: Vec<u8>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Mouse move (no button).
    MouseMove {
        x: u16,
        y: u16,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        id: Option<String>,
        /// Re-review P0.9: expected-state guard. Validated atomically with
        /// the send; on drift the action is refused (`stale_state`), never
        /// misdirected.
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Resize the terminal.
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        guard: Option<MutationGuardParam>,
    },
    /// Send a signal to the child process group.
    Signal {
        signal: i32,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        no_wait: Option<bool>,
        #[serde(default)]
        completion: Option<TuiCompletionParam>,
        #[serde(default)]
        wait_ms: Option<u64>,
        #[serde(default)]
        guard: Option<MutationGuardParam>,
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
            TuiActRequest::Resize { no_wait, .. } => *no_wait,
            TuiActRequest::Signal { no_wait, .. } => *no_wait,
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
            TuiActRequest::Resize { wait_ms, .. } => *wait_ms,
            TuiActRequest::Signal { wait_ms, .. } => *wait_ms,
        }
    }

    /// Optional completion override (re-review P0.1/P0.10): every canonical
    /// action — Resize and Signal included — can declare what "done" means.
    /// `signal: 15` + `{"type":"...","completion":"process_exit"}` is the
    /// ordinary way to express "kill and wait for exit"; `resize` +
    /// `{"type":"stable_screen","quiet_ms":400}` waits out reflow. The
    /// completion spec is self-contained, so the conversion is lossless.
    pub fn completion(&self) -> Option<crate::capture::CompletionPolicy> {
        match self {
            TuiActRequest::Key { completion, .. } => completion.clone(),
            TuiActRequest::Keys { completion, .. } => completion.clone(),
            TuiActRequest::Type { completion, .. } => completion.clone(),
            TuiActRequest::Paste { completion, .. } => completion.clone(),
            TuiActRequest::Raw { completion, .. } => completion.clone(),
            TuiActRequest::MouseClick { completion, .. } => completion.clone(),
            TuiActRequest::MousePress { completion, .. } => completion.clone(),
            TuiActRequest::MouseRelease { completion, .. } => completion.clone(),
            TuiActRequest::MouseMove { completion, .. } => completion.clone(),
            TuiActRequest::MouseDrag { completion, .. } => completion.clone(),
            TuiActRequest::MouseScroll { completion, .. } => completion.clone(),
            TuiActRequest::Resize { completion, .. } => completion.clone(),
            TuiActRequest::Signal { completion, .. } => completion.clone(),
        }
        .map(|c| c.to_policy())
    }

    /// The quiet-window override declared by the request's completion spec
    /// (`{"type":"stable_screen","quiet_ms":400}`), when present. The act
    /// handlers feed this into their quiet derivation so the wait length
    /// travels with the completion.
    pub fn completion_quiet_ms(&self) -> Option<u64> {
        match self {
            TuiActRequest::Key { completion, .. }
            | TuiActRequest::Keys { completion, .. }
            | TuiActRequest::Type { completion, .. }
            | TuiActRequest::Paste { completion, .. }
            | TuiActRequest::Raw { completion, .. }
            | TuiActRequest::MouseClick { completion, .. }
            | TuiActRequest::MousePress { completion, .. }
            | TuiActRequest::MouseRelease { completion, .. }
            | TuiActRequest::MouseMove { completion, .. }
            | TuiActRequest::MouseDrag { completion, .. }
            | TuiActRequest::MouseScroll { completion, .. }
            | TuiActRequest::Resize { completion, .. }
            | TuiActRequest::Signal { completion, .. } => {
                completion.as_ref().and_then(|c| c.quiet_ms())
            }
        }
    }

    /// The request's expected-state guard (re-review P0.9), if any. Shared
    /// field on every canonical action.
    pub fn guard(&self) -> Option<&MutationGuardParam> {
        match self {
            TuiActRequest::Key { guard, .. }
            | TuiActRequest::Keys { guard, .. }
            | TuiActRequest::Type { guard, .. }
            | TuiActRequest::Paste { guard, .. }
            | TuiActRequest::Raw { guard, .. }
            | TuiActRequest::MouseClick { guard, .. }
            | TuiActRequest::MousePress { guard, .. }
            | TuiActRequest::MouseRelease { guard, .. }
            | TuiActRequest::MouseMove { guard, .. }
            | TuiActRequest::MouseDrag { guard, .. }
            | TuiActRequest::MouseScroll { guard, .. }
            | TuiActRequest::Resize { guard, .. }
            | TuiActRequest::Signal { guard, .. } => guard.as_ref(),
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
