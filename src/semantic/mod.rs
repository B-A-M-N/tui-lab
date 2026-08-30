//! Semantic screen model (spec section 8/9/28). Analyzes the grid and derives
//! probable UI structure. Every inferred object carries `confidence`, `evidence`,
//! and `source = "inferred"` so heuristic interpretation is never presented as
//! ground truth. When a framework adapter supplies native info, source = "native"
//! with confidence 1.0.

pub mod affordance;
pub mod components;
pub mod confidence;
pub mod controls;
pub mod focus;
pub mod recognizers;
pub mod regions;
pub mod relationships;
pub mod state_tree;

pub use affordance::{infer_affordances, Affordance, Invocation, Visibility};

pub use components::{
    detect_components, Component, ScrollbarComponent, ScrollbarOrientation, TableComponent,
    TreeComponent,
};
pub use confidence::Confidence;
pub use controls::{Control, ControlKind};
pub use focus::FocusInfo;
pub use regions::{ClippingState, Region, RegionKind};
pub use relationships::{Relationship, RelationshipEngine, SemanticRelation};
pub use state_tree::{build_state_tree, ControlEntry, StateNode, TerminalStateTree};

use crate::screen::ScreenState;

/// Full semantic analysis of one screen.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SemanticScreen {
    pub cols: u16,
    pub rows: u16,
    pub regions: Vec<Region>,
    pub controls: Vec<Control>,
    pub focus: FocusInfo,
    /// Normalized spatial relations between controls and regions, keyed by
    /// stable IDs (see [`SemanticRelation`]).
    pub relationships: Vec<SemanticRelation>,
    /// Actionable capabilities with their visible cues (re-review Wave-4).
    /// Empty list ≠ no capabilities: bindings may exist without any on-screen
    /// hint, which is exactly what the discoverability audit must flag.
    pub affordances: Vec<Affordance>,
    /// Screen-level components (tables, trees, scrollbars — re-review
    /// Wave-4): multi-row widgets with structural evidence, beyond the
    /// per-line [`Self::controls`].
    pub components: Vec<Component>,
}

/// Build a semantic screen from a raw [`ScreenState`].
pub fn analyze(screen: &ScreenState) -> SemanticScreen {
    let regions = regions::detect_regions(screen);
    let controls = controls::detect_controls(screen, &regions);
    let focus = focus::infer_focus(screen, &controls);
    let relationships = RelationshipEngine::find_region_relationships(&controls, &regions);
    let affordances = affordance::infer_affordances(screen, &controls);
    let components = components::detect_components(screen);
    SemanticScreen {
        cols: screen.cols,
        rows: screen.rows,
        regions,
        controls,
        focus,
        relationships,
        affordances,
        components,
    }
}
