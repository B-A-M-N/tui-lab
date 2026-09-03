//! tui_explore parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_explore` mode.
    ExploreMode;
    [
        Random => "random", GuidedCandidates => "guided_candidates",
        Semantic => "semantic", StateGraph => "state_graph",
    ]
);

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiExploreParams {
    pub mode: Known<ExploreMode>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub actions: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub recording_path: Option<String>,
    /// guided_candidates: highest risk class the caller accepts
    /// (`safe` < `mutating` < `destructive` < `external_side_effect`).
    /// Candidates above the allowance are filtered, never merely flagged
    /// (Wave D item 33). Random/semantic exploration use it too (items
    /// 27/28): pool actions above the class are excluded before any draw
    /// — `mutating` (the default) keeps Escape (unknown) out of the pool.
    #[serde(default)]
    pub max_risk: Option<String>,
}
