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
    /// Optional session id. When given, `detect`/`capabilities` ALSO report
    /// the native-channel facts that only a live session can attest:
    /// whether the adapter file exists, whether this app actually
    /// cooperated (wrote ≥1 valid frame), and the frame health split.
    #[serde(default)]
    pub id: Option<String>,
}
