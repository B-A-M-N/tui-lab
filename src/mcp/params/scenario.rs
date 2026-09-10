//! tui_scenario / tui_record parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_scenario` action.
    ScenarioAction;
    [
        List => "list", RecordStart => "record_start", RecordStop => "record_stop",
        Save => "save", Export => "export", Run => "run",
        RegressionAsset => "regression_asset",
    ]
);

selector_enum!(
    /// `tui_scenario action=regression_asset` asset kind (finding 39). The
    /// generator emits only asset kinds the finding's own evidence can
    /// justify; an unavailable kind is refused with the reason, never
    /// synthesized from nothing.
    RegressionAssetType;
    [
        Scenario => "scenario", Assertion => "assertion",
        ContractRule => "contract_rule", ViewportCase => "viewport_case",
    ]
);

selector_enum!(
    /// `tui_record` format / lifecycle selector.
    RecordFormat;
    [ Start => "start", Stop => "stop", Cast => "cast", Svg => "svg", Png => "png" ]
);

selector_enum!(
    /// Replay failure policy (audit finding 5): what happens after a step
    /// fails.
    ScenarioFailurePolicy;
    [ Stop => "stop", Continue => "continue" ]
);

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
    /// Sensitive-parameter values for `run` (audit P0-18): `{"PASSWORD":
    /// "..."}` substitutes the scenario's declared `${NAME}` references at
    /// replay time. Values are used in-memory only — they are never
    /// persisted into the scenario, the run ledger, or any artifact.
    #[serde(default)]
    pub parameters: Option<std::collections::BTreeMap<String, String>>,
    /// Repeat count for `run` (P1-23). The scenario is replayed up to this
    /// many times on the same session and classified as stable or flaky.
    /// Default 1 (one-shot). Values >1 stop on the first stable verdict:
    /// stable pass only after all runs pass, stable failure only after all
    /// fail; otherwise the verdict is flaky.
    #[serde(default)]
    pub repeat: Option<u32>,
    /// Failure policy override for `run` (audit finding 5): `stop` (the
    /// default) halts at the first failed step and skips the rest;
    /// `continue` runs every step. Overrides the scenario's recorded
    /// `on_failure` for this replay only.
    #[serde(default)]
    pub on_failure: Option<Known<ScenarioFailurePolicy>>,
    /// regression_asset (finding 39): the finding whose evidence the asset
    /// is synthesized from (from tui_audit or tui://findings).
    #[serde(default)]
    pub finding_id: Option<String>,
    /// regression_asset: which asset kind to synthesize. Omitted → the
    /// generator emits every kind the finding's evidence can justify.
    #[serde(default)]
    pub asset_type: Option<Known<RegressionAssetType>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRecordParams {
    #[serde(default)]
    pub format: Option<Known<RecordFormat>>,
    #[serde(default)]
    pub id: Option<String>,
}
