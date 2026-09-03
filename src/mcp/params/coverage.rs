//! tui_coverage parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiCoverageParams {
    #[serde(default)]
    pub action: Option<Known<CoverageAction>>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    /// `action=delta` (review §13): read targets first seen AFTER this
    /// sequence number and return the new cursor in the response. Caller-
    /// owned, so two agents asking for delta never consume each other's
    /// window. When omitted, the run's own cursor is used and advanced —
    /// the historical behavior, still fine for a single consumer.
    #[serde(default)]
    pub since_seq: Option<u64>,
}
