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
    /// Native semantic channel revision (beta rereview P0-1). A native-only
    /// focus/state update can change the UI without changing the grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_revision: Option<u64>,
}

impl MutationGuardParam {
    /// Convert to the executor guard.
    pub fn to_guard(&self) -> crate::execution::MutationGuard {
        crate::execution::MutationGuard {
            generation: self.generation,
            structure_hash: self.structure_hash.clone(),
            focus_control_id: self.focus_control_id.clone(),
            native_revision: self.native_revision,
        }
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

    /// The runtime `CompletionPolicy` this name stands for.
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
    /// The runtime `CompletionPolicy` this spec stands for. The text
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
    /// The runtime `CompletionPolicy` this wire value stands for.
    pub fn to_policy(&self) -> crate::capture::CompletionPolicy {
        match self {
            TuiCompletionParam::Name(n) => n.to_policy(),
            TuiCompletionParam::Spec(s) => s.to_policy(),
        }
    }

    /// The canonical wire name for this completion, so recorded scenarios
    /// can persist it losslessly.
    pub fn name(&self) -> &'static str {
        match self {
            TuiCompletionParam::Name(n) => n.as_str(),
            TuiCompletionParam::Spec(s) => match s {
                CompletionSpec::StableScreen { .. } => "stable_screen",
                CompletionSpec::TextAppears { .. } => "text_appears",
                CompletionSpec::TextDisappears { .. } => "text_disappears",
            },
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

/// Beta-audit P0.9: the per-action payloads are the SINGLE source of
/// truth. [`TuiActRequest`] wraps them as newtype variants (the serde
/// wire format is unchanged — internally-tagged with `flatten`), and the
/// schema is composed from the SAME structs, so a field added to a
/// payload appears in both the parser and the advertised schema or
/// compilation fails. The old hand-maintained mirror enum drifted within
/// weeks: its `resize`/`signal` variants lacked the `guard` field the
/// real request accepted, so the advertised API rejected calls the
/// server actually supported.
///
/// Shared transport fields on every action: how to observe completion,
/// which session, and the expected-state guard.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ActCommon {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_wait: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<TuiCompletionParam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// Explicit completion budget ceiling (beta audit P0.6). Omitted means
    /// the historical default (`wait_ms + 1000ms`). It is never silently
    /// expanded; call it through the canonical executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_budget_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Re-review P0.9: expected-state guard. Validated atomically with
    /// the send; on drift the action is refused (`stale_state`), never
    /// misdirected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<MutationGuardParam>,
}

impl ActCommon {
    /// All transport fields absent — the ordinary action.
    pub fn none() -> Self {
        ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        }
    }
}

macro_rules! act_payload {
    ($(#[$meta:meta])* $name:ident { $($rest:tt)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
        pub struct $name {
            #[serde(flatten)]
            pub common: ActCommon,
            $($rest)*
        }
    };
}

act_payload!(
    /// `{"action":"key","key":"tab"}` — one key event.
    KeyPayload {
        pub key: String,
    }
);
act_payload!(
    /// `{"action":"keys","keys":["ctrl+a","x"]}` — a key sequence.
    KeysPayload {
        pub keys: Vec<String>,
    }
);
act_payload!(
    /// `{"action":"type","text":"..."}` — type text. `sensitive` marks
    /// the payload for redaction downstream (audit item 28).
    TypePayload {
        pub text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sensitive: Option<bool>,
    }
);
act_payload!(
    /// `{"action":"paste","paste":"..."}` — paste text (bracketed when
    /// negotiated).
    PastePayload {
        pub paste: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sensitive: Option<bool>,
    }
);
act_payload!(
    /// `{"action":"raw","raw":[..]}` — raw bytes.
    RawPayload {
        pub raw: Vec<u8>,
    }
);
act_payload!(
    /// Mouse payloads share position + optional button.
    MouseClickPayload {
        pub x: u16,
        pub y: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<MouseButtonParam>,
    }
);
act_payload!(
    MousePressPayload {
        pub x: u16,
        pub y: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<MouseButtonParam>,
    }
);
act_payload!(
    MouseReleasePayload {
        pub x: u16,
        pub y: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<MouseButtonParam>,
    }
);
act_payload!(
    MouseMovePayload {
        pub x: u16,
        pub y: u16,
    }
);
act_payload!(
    MouseDragPayload {
        pub x: u16,
        pub y: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<MouseButtonParam>,
    }
);
act_payload!(
    MouseScrollPayload {
        pub x: u16,
        pub y: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub direction: Option<ScrollDirectionParam>,
    }
);
act_payload!(
    /// `{"action":"resize","cols":..,"rows":..}`.
    ResizePayload {
        pub cols: u16,
        pub rows: u16,
    }
);
act_payload!(
    /// `{"action":"signal","signal":15}` — signal the child's process
    /// group.
    SignalPayload {
        pub signal: i32,
    }
);

/// Schema wrapper for [`TuiActRequest`]: re-roots the enum's natural
/// `oneOf` schema as `{"type":"object","oneOf":[...]}` so the MCP
/// inputSchema passes rmcp's root-object validation (MCP spec). The
/// variant list is composed from the SAME payload structs the enum wraps
/// (beta-audit P0.9) — the old hand-maintained mirror enum is gone.
///
/// Serde deserialization of `TuiActRequest` is untouched.
#[doc(hidden)]
pub struct TuiActRequestSchema;

impl TuiActRequestSchema {
    pub fn wrapped(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        use schemars::json_schema;
        let variants = <TuiActRequest as schemars::JsonSchema>::json_schema(gen);
        json_schema!({
            "type": "object",
            "oneOf": variants,
        })
    }
}

/// Tagged enum for `tui_act` requests (spec item 30).
///
/// Each variant carries exactly the fields needed for that action — no more,
/// no less. This gives Hermes a meaningful schema instead of one giant struct
/// where any field can appear with any action.
///
/// Beta-audit P0.9: every variant is a newtype over a payload struct that
/// also generates the schema, so parser and advertised schema cannot drift.
/// The wire format is the historical internally-tagged shape
/// (`{"action":"key","key":"tab",...}`) — `flatten` preserves it exactly.
///
/// The MCP inputSchema root is wrapped as `type: object` (see
/// `TuiActRequestSchema`) because rmcp 3.1.4 requires root `type: object`
/// (MCP spec); serde's internally-tagged enum alone generates a bare `oneOf`
/// there, which panicked the tool router on every stdio `tools/list` /
/// `tools/call`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TuiActRequest {
    /// Send a single key event (e.g. "enter", "ctrl+c", "tab").
    Key(KeyPayload),
    /// Send a sequence of key events.
    Keys(KeysPayload),
    /// Type text into the terminal.
    Type(TypePayload),
    /// Paste text (with bracketed paste escape if negotiated).
    Paste(PastePayload),
    /// Send raw bytes.
    Raw(RawPayload),
    /// Mouse click (press + release).
    MouseClick(MouseClickPayload),
    /// Mouse press only.
    MousePress(MousePressPayload),
    /// Mouse release only.
    MouseRelease(MouseReleasePayload),
    /// Mouse move (no button).
    MouseMove(MouseMovePayload),
    /// Mouse drag.
    MouseDrag(MouseDragPayload),
    /// Mouse scroll.
    MouseScroll(MouseScrollPayload),
    /// Resize the terminal.
    Resize(ResizePayload),
    /// Send a signal to the child process group.
    Signal(SignalPayload),
}

impl schemars::JsonSchema for TuiActRequest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("TuiActRequest")
    }

    fn json_schema(gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // The inputSchema ROOT is this schema (rmcp takes the parameter
        // type's schema verbatim), so the historical root wrapping
        // (`type: object` around the oneOf — rmcp 3.1.4 panics on a bare
        // oneOf root) happens here, in one place.
        use schemars::json_schema;
        // One object schema per payload: the `action` tag const + the
        // payload's flattened properties, all composed from the payload
        // structs themselves. A field added to a payload (like the guard
        // the old mirror dropped from resize/signal) lands here by
        // construction.
        let variant = |tag: &'static str, payload: schemars::Schema| {
            let mut obj = json_schema!({ "type": "object" });
            let map = obj.ensure_object();
            let inner = payload.to_value();
            // A payload struct's schema is a flat object schema (its fields
            // are flattened in), so its entries merge directly; the type
            // wrapper's own "type" wins.
            if let Some(inner_obj) = inner.as_object() {
                for (k, v) in inner_obj {
                    if k == "$ref" || k == "title" {
                        continue; // no refs: payloads are inline objects
                    }
                    if k == "type" {
                        continue; // our object type wins
                    }
                    map.insert(k.clone(), v.clone());
                }
            }
            let props = map
                .entry("properties")
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let Some(p) = props.as_object_mut() {
                p.insert(
                    "action".to_string(),
                    serde_json::json!({ "const": tag, "type": "string" }),
                );
            }
            let req = map
                .entry("required")
                .or_insert_with(|| serde_json::Value::Array(Default::default()));
            if let Some(r) = req.as_array_mut() {
                if !r.iter().any(|v| v == "action") {
                    r.push(serde_json::json!("action"));
                }
            }
            obj
        };
        let one_of = vec![
            variant(
                "key",
                <KeyPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "keys",
                <KeysPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "type",
                <TypePayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "paste",
                <PastePayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "raw",
                <RawPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_click",
                <MouseClickPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_press",
                <MousePressPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_release",
                <MouseReleasePayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_move",
                <MouseMovePayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_drag",
                <MouseDragPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "mouse_scroll",
                <MouseScrollPayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "resize",
                <ResizePayload as schemars::JsonSchema>::json_schema(gen),
            ),
            variant(
                "signal",
                <SignalPayload as schemars::JsonSchema>::json_schema(gen),
            ),
        ];
        json_schema!({
            "type": "object",
            "oneOf": one_of,
        })
    }
}

/// Helper to extract common fields from any act variant.
impl TuiActRequest {
    pub fn no_wait(&self) -> bool {
        match self {
            TuiActRequest::Key(p) => p.common.no_wait,
            TuiActRequest::Keys(p) => p.common.no_wait,
            TuiActRequest::Type(p) => p.common.no_wait,
            TuiActRequest::Paste(p) => p.common.no_wait,
            TuiActRequest::Raw(p) => p.common.no_wait,
            TuiActRequest::MouseClick(p) => p.common.no_wait,
            TuiActRequest::MousePress(p) => p.common.no_wait,
            TuiActRequest::MouseRelease(p) => p.common.no_wait,
            TuiActRequest::MouseMove(p) => p.common.no_wait,
            TuiActRequest::MouseDrag(p) => p.common.no_wait,
            TuiActRequest::MouseScroll(p) => p.common.no_wait,
            TuiActRequest::Resize(p) => p.common.no_wait,
            TuiActRequest::Signal(p) => p.common.no_wait,
        }
        .unwrap_or(false)
    }

    /// Shared transport fields, regardless of action variant.
    pub fn common(&self) -> &ActCommon {
        match self {
            TuiActRequest::Key(p) => &p.common,
            TuiActRequest::Keys(p) => &p.common,
            TuiActRequest::Type(p) => &p.common,
            TuiActRequest::Paste(p) => &p.common,
            TuiActRequest::Raw(p) => &p.common,
            TuiActRequest::MouseClick(p) => &p.common,
            TuiActRequest::MousePress(p) => &p.common,
            TuiActRequest::MouseRelease(p) => &p.common,
            TuiActRequest::MouseMove(p) => &p.common,
            TuiActRequest::MouseDrag(p) => &p.common,
            TuiActRequest::MouseScroll(p) => &p.common,
            TuiActRequest::Resize(p) => &p.common,
            TuiActRequest::Signal(p) => &p.common,
        }
    }

    /// Caller-supplied completion ceiling, when declared.
    pub fn settle_budget_ms(&self) -> Option<u64> {
        self.common().settle_budget_ms
    }

    pub fn wait_ms(&self) -> Option<u64> {
        match self {
            TuiActRequest::Key(p) => p.common.wait_ms,
            TuiActRequest::Keys(p) => p.common.wait_ms,
            TuiActRequest::Type(p) => p.common.wait_ms,
            TuiActRequest::Paste(p) => p.common.wait_ms,
            TuiActRequest::Raw(p) => p.common.wait_ms,
            TuiActRequest::MouseClick(p) => p.common.wait_ms,
            TuiActRequest::MousePress(p) => p.common.wait_ms,
            TuiActRequest::MouseRelease(p) => p.common.wait_ms,
            TuiActRequest::MouseMove(p) => p.common.wait_ms,
            TuiActRequest::MouseDrag(p) => p.common.wait_ms,
            TuiActRequest::MouseScroll(p) => p.common.wait_ms,
            TuiActRequest::Resize(p) => p.common.wait_ms,
            TuiActRequest::Signal(p) => p.common.wait_ms,
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
            TuiActRequest::Key(p) => p.common.completion.clone(),
            TuiActRequest::Keys(p) => p.common.completion.clone(),
            TuiActRequest::Type(p) => p.common.completion.clone(),
            TuiActRequest::Paste(p) => p.common.completion.clone(),
            TuiActRequest::Raw(p) => p.common.completion.clone(),
            TuiActRequest::MouseClick(p) => p.common.completion.clone(),
            TuiActRequest::MousePress(p) => p.common.completion.clone(),
            TuiActRequest::MouseRelease(p) => p.common.completion.clone(),
            TuiActRequest::MouseMove(p) => p.common.completion.clone(),
            TuiActRequest::MouseDrag(p) => p.common.completion.clone(),
            TuiActRequest::MouseScroll(p) => p.common.completion.clone(),
            TuiActRequest::Resize(p) => p.common.completion.clone(),
            TuiActRequest::Signal(p) => p.common.completion.clone(),
        }
        .map(|c| c.to_policy())
    }

    /// The quiet-window override declared by the request's completion spec
    /// (`{"type":"stable_screen","quiet_ms":400}`), when present. The act
    /// handlers feed this into their quiet derivation so the wait length
    /// travels with the completion.
    pub fn completion_quiet_ms(&self) -> Option<u64> {
        match self {
            TuiActRequest::Key(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::Keys(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::Type(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::Paste(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::Raw(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::MouseClick(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::MousePress(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::MouseRelease(p) => {
                p.common.completion.as_ref().and_then(|c| c.quiet_ms())
            }
            TuiActRequest::MouseMove(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::MouseDrag(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::MouseScroll(p) => {
                p.common.completion.as_ref().and_then(|c| c.quiet_ms())
            }
            TuiActRequest::Resize(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
            TuiActRequest::Signal(p) => p.common.completion.as_ref().and_then(|c| c.quiet_ms()),
        }
    }

    /// The request's expected-state guard (re-review P0.9), if any. Shared
    /// field on every canonical action.
    pub fn guard(&self) -> Option<&MutationGuardParam> {
        match self {
            TuiActRequest::Key(p) => p.common.guard.as_ref(),
            TuiActRequest::Keys(p) => p.common.guard.as_ref(),
            TuiActRequest::Type(p) => p.common.guard.as_ref(),
            TuiActRequest::Paste(p) => p.common.guard.as_ref(),
            TuiActRequest::Raw(p) => p.common.guard.as_ref(),
            TuiActRequest::MouseClick(p) => p.common.guard.as_ref(),
            TuiActRequest::MousePress(p) => p.common.guard.as_ref(),
            TuiActRequest::MouseRelease(p) => p.common.guard.as_ref(),
            TuiActRequest::MouseMove(p) => p.common.guard.as_ref(),
            TuiActRequest::MouseDrag(p) => p.common.guard.as_ref(),
            TuiActRequest::MouseScroll(p) => p.common.guard.as_ref(),
            TuiActRequest::Resize(p) => p.common.guard.as_ref(),
            TuiActRequest::Signal(p) => p.common.guard.as_ref(),
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            TuiActRequest::Key(p) => p.common.id.as_deref(),
            TuiActRequest::Keys(p) => p.common.id.as_deref(),
            TuiActRequest::Type(p) => p.common.id.as_deref(),
            TuiActRequest::Paste(p) => p.common.id.as_deref(),
            TuiActRequest::Raw(p) => p.common.id.as_deref(),
            TuiActRequest::MouseClick(p) => p.common.id.as_deref(),
            TuiActRequest::MousePress(p) => p.common.id.as_deref(),
            TuiActRequest::MouseRelease(p) => p.common.id.as_deref(),
            TuiActRequest::MouseMove(p) => p.common.id.as_deref(),
            TuiActRequest::MouseDrag(p) => p.common.id.as_deref(),
            TuiActRequest::MouseScroll(p) => p.common.id.as_deref(),
            TuiActRequest::Resize(p) => p.common.id.as_deref(),
            TuiActRequest::Signal(p) => p.common.id.as_deref(),
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
            TuiActRequest::Type(p) => p.sensitive.unwrap_or(false),
            TuiActRequest::Paste(p) => p.sensitive.unwrap_or(false),
            _ => false,
        }
    }
}
