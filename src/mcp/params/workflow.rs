//! tui_workflow parameters (finding 38: the construction-oriented
//! workflow object).

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

// `tui_workflow` action (finding 38). The tool JOINS existing primitives
// into one object per finding — it is not another autonomous agent: every
// field cites the run's own evidence, and `verify` only replays what the
// finding's own reproduction names.
//
//   inspect  — one finding's full construction chain: component identity →
//              source refs → framework context → contract expectation →
//              minimal reproduction → targeted validation.
//   verify   — run the finding's verification plan LIVE: replay the
//              reproduction scenario (when present), then the targeted
//              re-checks; lease-gated (it drives the app).
//   diagnose — all findings' chains at once (list-level inspect; no driving).
selector_enum!(
    /// `tui_workflow` action (finding 38): inspect | verify | diagnose.
    WorkflowAction;
    [ Inspect => "inspect", Verify => "verify", Diagnose => "diagnose" ]
);

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiWorkflowParams {
    pub action: Known<WorkflowAction>,
    /// inspect/verify: the finding id (from tui_audit or tui://findings).
    /// Required for inspect and verify; diagnose ignores it.
    #[serde(default)]
    pub finding_id: Option<String>,
    /// verify: session to replay/drive against (defaults to the active
    /// one).
    #[serde(default)]
    pub id: Option<String>,
    /// inspect/diagnose: project directory for framework detection
    /// (defaults to the primary session's cwd).
    #[serde(default)]
    pub cwd: Option<String>,
    /// verify: authorize the INVASIVE parts of the verification (a
    /// re-check surface classified beyond observational). Default
    /// false: an invasive re-check is withheld and reported `gated`
    /// with this flag named — never silently downgraded and never a
    /// silent refusal to verify. True mirrors tui_audit's
    /// allow_mutation through the same centralized policy.
    #[serde(default)]
    pub allow_mutation: Option<bool>,
}
