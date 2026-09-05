//! tui_observe / tui_wait / tui_assert / tui_checkpoint parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_observe` mode.
    ObserveMode;
    [
        Summary => "summary", Screen => "screen", Cells => "cells",
        Semantic => "semantic", Tree => "tree", Nodes => "nodes",
        Diff => "diff", Changes => "changes", Scrollback => "scrollback",
        Search => "search", CommandState => "command_state", History => "history",
        Protocol => "protocol", Streams => "streams", TerminalModes => "terminal_modes",
        // Finding 36: the one-call construction/inspection view — frame
        // identity, semantic identity, per-control facts with stable ids +
        // source refs + affordances, and the loaded contract's verdict on
        // this frame.
        Inspect => "inspect",
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
            if !event_text(ev)
                .map(|t| t.contains(needle.as_str()))
                .unwrap_or(false)
            {
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
        K::Bell
        | K::VisualChanged
        | K::FocusChanged { .. }
        | K::ProcessStarted
        | K::ProcessExited { .. }
        | K::SemanticChanged => return None,
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
    /// mode=inspect (finding 36): narrow the per-control list to one
    /// control — stable id (`button/save`), a unique label substring, or
    /// a native id (`#save-button`). Absent = every control.
    #[serde(default)]
    pub target: Option<String>,
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
