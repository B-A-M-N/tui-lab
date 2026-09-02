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
        Attach => "attach",
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
        Protocol => "protocol", Streams => "streams", TerminalModes => "terminal_modes",
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
        Event => "event",
    ]
);

/// Re-review item 17: a declarative predicate over the converged event
/// history. `tui_wait condition=event` blocks until the session's event
/// stream contains an event matching the predicate — "wait until the app
/// rang the bell", "wait until focus moved", "wait until anything at all
/// happened" — without polling loops in the caller. The predicate is
/// conjunctive: every supplied field must match.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EventPredicate {
    /// Keep only these event kinds (machine names from `tui_observe
    /// mode=history`: `output`, `screen_changed`, `visual_changed`,
    /// `cursor_moved`, `bell`, `title_changed`, `resize`,
    /// `focus_changed`, `process_started`, `process_exited`,
    /// `semantic_changed`, `native_event`).
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Substring the event's payload must contain — title text, native
    /// signal, native target, or the `event`/`target` pair of a native
    /// event.
    #[serde(default)]
    pub contains: Option<String>,
    /// Only events with `seq > this` count (a consumer cursor: "something
    /// NEW must happen", not "it happened once in the past").
    #[serde(default)]
    pub since_seq: Option<u64>,
}

impl EventPredicate {
    /// Does one event match? (Conjunctive over the supplied fields.)
    pub fn matches(&self, ev: &crate::events::TerminalEvent) -> bool {
        if let Some(floor) = self.since_seq {
            if ev.seq <= floor {
                return false;
            }
        }
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k == ev.kind.name()) {
            return false;
        }
        if let Some(needle) = &self.contains {
            if !event_text(ev).map(|t| t.contains(needle.as_str())).unwrap_or(false) {
                return false;
            }
        }
        true
    }
}

/// The searchable text of an event's payload (the `contains` needle looks
/// here). Events without a meaningful payload (bell, process edges, visual
/// changed) match no needle — a `contains` predicate over them can never
/// fire, which is honest rather than matching vacuously.
fn event_text(ev: &crate::events::TerminalEvent) -> Option<String> {
    use crate::events::TerminalEventKind as K;
    Some(match &ev.kind {
        K::TitleChanged { title } => title.clone(),
        K::NativeEvent { event, target } => format!("{event} {target}"),
        K::CursorMoved { x, y } => format!("{x} {y}"),
        K::Resize { cols, rows } => format!("{cols}x{rows}"),
        K::Output { byte_len } => byte_len.to_string(),
        K::ScreenChanged { dirty_rows } => dirty_rows
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        K::QueryAnswered { class } => class.clone(),
        K::Bell | K::VisualChanged | K::FocusChanged { .. } | K::ProcessStarted
        | K::ProcessExited { .. } | K::SemanticChanged => return None,
    })
}

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
    /// `tui_contract` mode override (re-review item 32): a typed selector
    /// so a typo'd mode (`"strcit"`) surfaces as envelope `invalid_request`
    /// naming the accepted set, never as a silently-ignored string.
    ContractModeParam;
    [ Advisory => "advisory", Validation => "validation", Strict => "strict" ]
);

selector_enum!(
    /// `tui_audit` profile. `layout` is a documented alias of `resize`;
    /// `contract` (Wave E) folds conformance into findings. `unicode`,
    /// `controls`, `rendering`, `input_protocol`, `shell_cli`, `lifecycle`
    /// and `terminal_modes` (Wave 3) are frame-level subsystem audits over
    /// the raw ring / one fused frame; `query_response` sends one device
    /// query (CSI 6n) and verifies the CPR answer.
    AuditProfile;
    [
        Full => "full", Keyboard => "keyboard", Focus => "focus", Resize => "resize",
        Layout => "layout", Clipping => "clipping", Discoverability => "discoverability",
        Navigation => "navigation", Contract => "contract", Color => "color",
        Performance => "performance", Mouse => "mouse", States => "states",
        Errors => "errors", Unicode => "unicode", Controls => "controls",
        TerminalModes => "terminal_modes", Rendering => "rendering",
        InputProtocol => "input_protocol", ShellCli => "shell_cli",
        Lifecycle => "lifecycle", QueryResponse => "query_response",
    ]
);

selector_enum!(
    /// `tui_coverage` action.
    ///
    /// Coverage is collected **continuously** (NativeSemanticProtocol
    /// `coverage` events fold into the run ledger on arrival); there is no
    /// instrumentation on/off phase, so `start`/`stop` deliberately do NOT
    /// exist — the review flagged the old no-op pair as theater. The run
    /// ledger views (`ledger`, `summary`, `collect`, `delta`) read accumulated
    /// evidence; `uncovered` is honestly `unsupported` until a denominator
    /// source exists.
    CoverageAction;
    [
        Detect => "detect", Summary => "summary", Collect => "collect",
        Delta => "delta", Uncovered => "uncovered", Ledger => "ledger",
    ]
);

selector_enum!(
    /// `tui_framework` action.
    FrameworkAction;
    [ Detect => "detect", Capabilities => "capabilities", AdapterSnippet => "adapter_snippet" ]
);

selector_enum!(
    /// `tui_run` action. `list` (Wave G item 75) enumerates persisted runs;
    /// `resume` (Wave G item 74) restores one as the server's live run;
    /// `repair` (audit item: vket/RepairPacket) returns repair bundles.
    RunAction;
    [
        Status => "status", Persist => "persist", Close => "close",
        Context => "context", List => "list", Resume => "resume",
        Repair => "repair",
        // Review P0.1/2: begin a fresh ephemeral run — the clean "next run"
        // operation. Starts a new evidence bundle; any live sessions from
        // the prior run become foreign owners (refused until stopped).
        New => "new",
        // Wave 5 item 46: one finding's repair packet joined with the
        // finding-baseline diff (before/after) — the "did the fix hold
        // without regressing anything?" bundle.
        Bundle => "bundle",
    ]
);

selector_enum!(
    /// `tui_contract` action.
    ContractAction;
    [ Load => "load", Validate => "validate", Status => "status", Compare => "compare",
      Scaffold => "scaffold", Baseline => "baseline" ]
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
    /// Engine selector (re-review P0): typed as `Known<BackendParam>` so an
    /// unknown name still reaches the envelope as `invalid_request` with the
    /// accepted list, matching the isolation param's contract.
    #[serde(default)]
    pub backend: Option<Known<BackendParam>>,
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
    /// action=attach (re-review item 18): the tmux target
    /// `session:window.pane` to adopt.
    #[serde(default)]
    pub target: Option<String>,
}

/// Engine vocabulary (re-review P0: exact engine selection). `auto` resolves
/// to the portable PTY engine for screen programs; `cli` and `pipe` pick the
/// line/pipe transports honestly instead of routing everything through the
/// one boolean-ish "backend" string the handler matched ad hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub enum BackendParam {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "portable_vt100")]
    PortableVt100,
    #[serde(rename = "cli")]
    Cli,
    #[serde(rename = "line_cli")]
    LineCli,
    #[serde(rename = "pipe")]
    Pipe,
    #[serde(rename = "tmux")]
    Tmux,
}

impl EnumVariants for BackendParam {
    const VARIANTS: &'static [&'static str] =
        &["auto", "portable_vt100", "cli", "line_cli", "pipe", "tmux"];
}

impl BackendParam {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendParam::Auto => "auto",
            BackendParam::PortableVt100 => "portable_vt100",
            BackendParam::Cli => "cli",
            BackendParam::LineCli => "line_cli",
            BackendParam::Pipe => "pipe",
            BackendParam::Tmux => "tmux",
        }
    }

    /// Resolve to the engine kind (`auto` → portable PTY).
    pub fn to_kind(self) -> crate::session::state::BackendKind {
        match self {
            BackendParam::Auto | BackendParam::PortableVt100 => {
                crate::session::state::BackendKind::PortableVt
            }
            BackendParam::Cli | BackendParam::LineCli => crate::session::state::BackendKind::PtyLine,
            BackendParam::Pipe => crate::session::state::BackendKind::Pipe,
            BackendParam::Tmux => crate::session::state::BackendKind::TmuxAttach,
        }
    }
}

impl std::str::FromStr for BackendParam {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(BackendParam::Auto),
            "portable_vt100" => Ok(BackendParam::PortableVt100),
            "cli" => Ok(BackendParam::Cli),
            "line_cli" => Ok(BackendParam::LineCli),
            "pipe" => Ok(BackendParam::Pipe),
            other => Err(format!(
                "unknown backend '{}' (expected one of: {})",
                other,
                Self::VARIANTS.join(", ")
            )),
        }
    }
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
    /// mode=history (re-review item 15): return events with `seq > since_seq`
    /// (0 = from the ring's retained start).
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// mode=history: return events with `seq <= until_seq`.
    #[serde(default)]
    pub until_seq: Option<u64>,
    /// mode=history: cap on returned events.
    #[serde(default)]
    pub limit: Option<u64>,
    /// mode=history: keep only these event kinds (machine names:
    /// "bell", "screen_changed", "native_event", "process_exited", …).
    #[serde(default)]
    pub event_types: Option<Vec<String>>,
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
            | TuiActRequest::Signal { completion, .. } => completion.as_ref().and_then(|c| c.quiet_ms()),
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

/// `tui_probe` stimulus (re-review item 12): the FULL canonical action
/// grammar — the same `TuiActRequest` shapes `tui_act` takes — plus the
/// legacy `{kind: key|type|click|none}` compact form (kept deserializable so
/// existing callers and recorded probes keep working). One action vocabulary
/// across `tui_act`, `tui_probe`, scenario, and exploration: the probe's old
/// local key parser (`{"kind":"key","key":"c","ctrl":true}`) had already
/// drifted from the canonical one, which is exactly the class of divergence
/// this unification removes. `{"action":"none"}` (or `{"kind":"none"}`) is a
/// drift probe.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ProbeStimulus {
    /// A full canonical action request — identical wire shape to `tui_act`.
    Canonical(TuiActRequest),
    /// The legacy compact form.
    Legacy(LegacyStimulus),
}

impl ProbeStimulus {
    /// The canonical action for this stimulus; `None` = drift probe (only
    /// the legacy `none` shape means that — a canonical request always
    /// names a real action).
    pub fn to_action(&self) -> Option<crate::execution::CanonicalAction> {
        match self {
            ProbeStimulus::Canonical(req) => crate::execution::CanonicalAction::from_request(req).ok(),
            ProbeStimulus::Legacy(l) => l.to_action(),
        }
    }

    /// The executor guard the stimulus may carry (canonical form only).
    pub fn guard(&self) -> Option<crate::execution::MutationGuard> {
        match self {
            ProbeStimulus::Canonical(req) => req.guard().map(|g| g.to_guard()),
            ProbeStimulus::Legacy(_) => None,
        }
    }
}

/// `tui_probe` legacy compact stimulus vocabulary. Retained for wire
/// compatibility; new callers send canonical actions.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyStimulus {
    /// A single named key, optionally modified (`key: "enter"`,
    /// `key: "ctrl+c"`, `key: "tab"`).
    Key {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
    },
    /// Type literal text.
    Type { text: String },
    /// Mouse click at cell coordinates.
    Click { button: MouseButtonParam, x: u16, y: u16 },
    /// No stimulus — observe drift between two settled frames.
    None,
}

impl LegacyStimulus {
    /// Convert to the canonical action (None stays None at the caller).
    pub fn to_action(&self) -> Option<crate::execution::CanonicalAction> {
        use crate::backend::{KeyModifiers, MouseButton};
        use crate::execution::CanonicalAction as CA;
        match self {
            LegacyStimulus::None => None,
            LegacyStimulus::Key { key, ctrl, alt, shift } => {
                let mut mods = KeyModifiers::empty();
                if *ctrl { mods |= KeyModifiers::CTRL; }
                if *alt { mods |= KeyModifiers::ALT; }
                if *shift { mods |= KeyModifiers::SHIFT; }
                let code = parse_key_name(key)?;
                Some(CA::Key { key: crate::backend::KeyEvent { code, modifiers: mods } })
            }
            LegacyStimulus::Type { text } => Some(CA::Type { text: text.clone() }),
            LegacyStimulus::Click { button, x, y } => Some(CA::MouseClick {
                button: match button {
                    MouseButtonParam::Left => MouseButton::Left,
                    MouseButtonParam::Middle => MouseButton::Middle,
                    MouseButtonParam::Right => MouseButton::Right,
                },
                x: *x,
                y: *y,
            }),
        }
    }
}

/// Key-name parser for the probe stimulus vocabulary (the ergonomic subset:
/// named keys + single characters).
fn parse_key_name(name: &str) -> Option<crate::backend::KeyCode> {
    use crate::backend::KeyCode;
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Escape,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        other => {
            let mut chars = other.chars();
            let single = chars.next()?;
            if chars.next().is_none() {
                KeyCode::Char(single)
            } else {
                return None;
            }
        }
    })
}

// `tui_probe` completion vocabulary: how the probe decides "after".
selector_enum!(
    /// Probe completion (a deliberately small closed set of the canonical
    /// [`crate::capture::CompletionPolicy`] shapes an experiment needs).
    ProbeCompletion;
    [
        Stable => "stable", FirstChange => "first_change",
        AnyChange => "any_change", TextAppears => "text_appears",
        TextDisappears => "text_disappears", ProcessExit => "process_exit",
        SemanticChange => "semantic_change", MayBeSilent => "may_be_silent",
    ]
);

impl ProbeCompletion {
    /// Convert to the canonical completion policy.
    pub fn to_policy(&self, text: Option<&str>) -> Option<crate::capture::CompletionPolicy> {
        use crate::capture::CompletionPolicy as CP;
        Some(match self {
            ProbeCompletion::Stable => CP::StableScreen,
            ProbeCompletion::FirstChange => CP::FirstScreenChange,
            ProbeCompletion::AnyChange => CP::AnyObservableChange,
            ProbeCompletion::TextAppears => CP::TextAppears(text?.to_string()),
            ProbeCompletion::TextDisappears => CP::TextDisappears(text?.to_string()),
            ProbeCompletion::ProcessExit => CP::ProcessExit,
            ProbeCompletion::SemanticChange => CP::SemanticChange,
            ProbeCompletion::MayBeSilent => CP::MayBeSilent,
        })
    }
}

/// Parameters for `tui_probe` (re-review Wave-2: the troubleshooting
/// primitive is an agent-visible tool — "try this and tell me EVERYTHING
/// materially different", with causal event scoping and the settled frame).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiProbeParams {
    /// The experiment: any canonical action (`{"action": ...}` — the exact
    /// `tui_act` grammar) or the legacy compact `{"kind": ...}` shape;
    /// omit (or `{"kind":"none"}`) for a drift probe.
    #[serde(default)]
    pub stimulus: Option<ProbeStimulus>,
    /// How "after" is decided. Either the legacy bare name
    /// (`"stable"`, `"text_appears"`, …) or a capture spec object
    /// (re-review item 13): `{"strategy":"frames","count":8}` to grab the
    /// first N frames of a transition ("press Enter and show me the first
    /// 8 frames"), `{"strategy":"after_duration","ms":250}` to sample the
    /// screen a fixed interval after the stimulus.
    #[serde(default)]
    pub completion: Option<Known<ProbeCompletion>>,
    /// The structured capture spec (item 13). Takes precedence over
    /// `completion` when present.
    #[serde(default)]
    pub capture: Option<ProbeCapture>,
    /// Required text for `text_appears` / `text_disappears`.
    #[serde(default)]
    pub text: Option<String>,
    /// Which watched aspects to surface as anomalies. Defaults to focus,
    /// controls, regions, cursor.
    #[serde(default)]
    pub watch: Option<Vec<Known<ProbeWatchParam>>>,
    /// Quiet interval for `stable` (ms). Default 120.
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    /// Overall ceiling so a never-settling TUI still returns. Default 5000.
    #[serde(default)]
    pub budget_ms: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
}

/// The probe's capture strategies (re-review item 13): how the after-side
/// of the experiment is collected. The completion names decide *when the
/// action is done*; the frames/duration strategies decide *what to record*
/// — "the first N frames of the transition" is a capture question, not a
/// completion question, and the old enum could not ask it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum ProbeCapture {
    /// Collect the first `count` distinct frames after the stimulus
    /// (the terminal microscope: flicker/double-draw diagnosis).
    Frames {
        /// How many distinct post-stimulus frames to record.
        count: usize,
    },
    /// Let the stimulus settle, then sample one frame after `ms`.
    AfterDuration {
        /// Delay between the stimulus and the sampled frame.
        ms: u64,
    },
}

impl ProbeCapture {
    /// The capture's budget share of the overall probe budget (frames
    /// want most of it; a duration sample needs only its delay plus
    /// settle slack).
    pub fn budget_hint(&self, total_ms: u64) -> u64 {
        match self {
            ProbeCapture::Frames { .. } => total_ms,
            ProbeCapture::AfterDuration { ms } => ms.saturating_add(1000).min(total_ms),
        }
    }
}

selector_enum!(
    /// Watched probe aspects.
    ProbeWatchParam;
    [ Cursor => "cursor", Focus => "focus", Style => "style",
      Controls => "controls", Regions => "regions", Process => "process" ]
);

impl ProbeWatchParam {
    /// Convert to the diagnostic watch aspect.
    pub fn to_watch(self) -> crate::diagnostic::ProbeWatch {
        use crate::diagnostic::ProbeWatch as PW;
        match self {
            ProbeWatchParam::Cursor => PW::Cursor,
            ProbeWatchParam::Focus => PW::Focus,
            ProbeWatchParam::Style => PW::Style,
            ProbeWatchParam::Controls => PW::Controls,
            ProbeWatchParam::Regions => PW::Regions,
            ProbeWatchParam::Process => PW::Process,
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
    /// `condition=event` (re-review item 17): the declarative event
    /// predicate to wait for.
    #[serde(default)]
    pub event: Option<EventPredicate>,
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
    /// list: base to scan (same resolution order). resume: base under which
    /// the run lives.
    #[serde(default)]
    pub root: Option<String>,
    /// close: kill sessions too? (default false — close never touches
    /// sessions unless explicitly told to).
    #[serde(default)]
    pub kill_sessions: Option<bool>,
    /// resume: the run id to restore (from `tui_run action=list`). Either
    /// this or `run_dir` must be given.
    #[serde(default)]
    pub run_id: Option<String>,
    /// resume: the run directory directly (as listed by `action=list`'s
    /// `dir` field) — for callers that already hold the path.
    #[serde(default)]
    pub run_dir: Option<String>,
    /// bundle: the finding id to bundle (Wave 5 item 46).
    #[serde(default)]
    pub finding_id: Option<String>,
    /// bundle: the labeled baseline to diff against (the audit pass the
    /// fix is being verified against; from tui_audit label=...).
    #[serde(default)]
    pub compare_to: Option<String>,
    /// resume: stop any live sessions that belong to a DIFFERENT run before
    /// restoring? (default false — resume refuses when foreign sessions are
    /// still live, to preserve run provenance; pass true to detach them).
    #[serde(default)]
    pub detach_existing_sessions: Option<bool>,
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
    /// (Wave D item 33). Random/semantic exploration use it too (items
    /// 27/28): pool actions above the class are excluded before any draw
    /// — `mutating` (the default) keeps Escape (unknown) out of the pool.
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
    /// Wave 4 item 36: mutation-safety selector. Default (absent/false)
    /// runs only observational profiles and reports invasive ones as
    /// withheld (ORCH-GATED). `true` runs everything.
    #[serde(default)]
    pub allow_mutation: Option<bool>,
    /// Wave 4 item 37: deep-audit mode — restart-replay between
    /// mutating drivers so each sees a fresh app (requires a session we
    /// launched; degrades honestly on attached sessions).
    #[serde(default)]
    pub deep_isolation: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiExplainParams {
    /// The finding id to explain (as produced/listed by `tui_audit` /
    /// `tui://findings`). Must reference a finding recorded in the current run.
    pub finding_id: String,
    /// Optional session id whose live terminal profile conditions the
    /// explanation (a capability the profile marks unverified bears on whether
    /// the finding is a real defect or an artifact of missing capability).
    #[serde(default)]
    pub id: Option<String>,
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
    /// load | validate | status | compare | scaffold
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
    /// Check-time mode override (re-review item 33, typed per item 32):
    /// advisory | validation | strict. Overrides the contract document's
    /// `schema.mode` for this check only — CI can run the same contract at
    /// both Validation (dev) and Strict (gate) without editing it.
    #[serde(default)]
    pub mode: Option<Known<ContractModeParam>>,
}
