//! tui_framework parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_framework` action.
    FrameworkAction;
    [ Detect => "detect", Capabilities => "capabilities", AdapterSnippet => "adapter_snippet" ]
);

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiFrameworkParams {
    pub action: Known<FrameworkAction>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}
