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
    /// Failure policy override for `run` (audit finding 5): `stop` (the
    /// default) halts at the first failed step and skips the rest;
    /// `continue` runs every step. Overrides the scenario's recorded
    /// `on_failure` for this replay only.
    #[serde(default)]
    pub on_failure: Option<Known<ScenarioFailurePolicy>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRecordParams {
    #[serde(default)]
    pub format: Option<Known<RecordFormat>>,
    #[serde(default)]
    pub id: Option<String>,
}
