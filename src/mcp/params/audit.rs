//! tui_audit / tui_explain parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_audit` profile. `layout` is a documented alias of `resize`;
    /// `contract` (Wave E) folds conformance into findings. `unicode`,
    /// `controls`, `rendering`, `input_protocol`, `shell_cli`, `lifecycle`
    /// and `terminal_modes` (Wave 3) are frame-level subsystem audits over
    /// the raw ring / one fused frame; `query_response` sends one device
    /// query (CSI 6n) and verifies the CPR answer. `lifecycle_exit` runs a
    /// real teardown audit: it EXITS and relaunches the target, so it
    /// additionally requires `allow_process_restart=true` (never implied
    /// by `allow_mutation`) and is never part of `profile=full`.
    AuditProfile;
    [
        Full => "full", Keyboard => "keyboard", Focus => "focus", Resize => "resize",
        Layout => "layout", Clipping => "clipping", Discoverability => "discoverability",
        Navigation => "navigation", Contract => "contract", Color => "color",
        Performance => "performance", Mouse => "mouse", States => "states",
        Errors => "errors", Unicode => "unicode", Controls => "controls",
        TerminalModes => "terminal_modes", Rendering => "rendering",
        InputProtocol => "input_protocol", ShellCli => "shell_cli",
        Lifecycle => "lifecycle", LifecycleExit => "lifecycle_exit",
        QueryResponse => "query_response",
    ]
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
    /// withheld (ORCH-GATED). `true` runs everything up to
    /// potentially-mutating drivers — but NOT process-consuming audits.
    #[serde(default)]
    pub allow_mutation: Option<bool>,
    /// Wave 4 item 37: deep-audit mode — restart-replay between
    /// mutating drivers so each sees a fresh app (requires a session we
    /// launched; degrades honestly on attached sessions).
    #[serde(default)]
    pub deep_isolation: Option<bool>,
    /// Explicit authorization for PROCESS-CONSUMING audits (`lifecycle_exit`):
    /// the profile will exit and relaunch the target. Deliberately NOT
    /// implied by `allow_mutation` — killing/restarting the app under audit
    /// is a stronger act than clicking its buttons — and `profile=full`
    /// never includes it. Deep isolation's restart-replay does not need
    /// this flag (it replays the session's own recorded LaunchSpec).
    #[serde(default)]
    pub allow_process_restart: Option<bool>,
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
