//! tui_contract parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_contract` mode override (re-review item 32): a typed selector
    /// so a typo'd mode (`"strcit"`) surfaces as envelope `invalid_request`
    /// naming the accepted set, never as a silently-ignored string.
    ContractModeParam;
    [ Advisory => "advisory", Validation => "validation", Strict => "strict" ]
);

selector_enum!(
    /// `tui_contract` action.
    ContractAction;
    [ Load => "load", Validate => "validate", Status => "status", Compare => "compare",
      Scaffold => "scaffold", Baseline => "baseline" ]
);

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
