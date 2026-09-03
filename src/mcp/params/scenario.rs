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
