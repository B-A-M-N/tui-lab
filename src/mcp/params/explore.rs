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

// The exploration risk allowance (audit P0-5): a TYPED selector, not a
// free-form string. A typo used to fall through `unwrap_or(Mutating)` —
// `"saef"` silently granted mutation permission. Unknown values are now
// answered with `invalid_request` naming the accepted set.
selector_enum!(
    /// Highest risk class the caller accepts
    /// (`safe` < `mutating` < `destructive` < `external_side_effect`;
    /// `unknown` is its own honest class, not a level).
    ExploreRisk;
    [
        Safe => "safe", Mutating => "mutating", Destructive => "destructive",
        ExternalSideEffect => "external_side_effect", Unknown => "unknown",
    ]
);

impl ExploreRisk {
    /// Convert to the engine risk class.
    pub fn to_risk(self) -> crate::intent::ActionRisk {
        match self {
            ExploreRisk::Safe => crate::intent::ActionRisk::Safe,
            ExploreRisk::Mutating => crate::intent::ActionRisk::Mutating,
            ExploreRisk::Destructive => crate::intent::ActionRisk::Destructive,
            ExploreRisk::ExternalSideEffect => crate::intent::ActionRisk::ExternalSideEffect,
            ExploreRisk::Unknown => crate::intent::ActionRisk::Unknown,
        }
    }
}

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
    /// 27/28): pool actions above the class are excluded before any draw.
    /// Defaults to `safe` (audit P0-5): mutation is EXPLICIT, never an
    /// untyped-string fallback.
    #[serde(default)]
    pub max_risk: Option<Known<ExploreRisk>>,
}
