//! Semantic screen model (spec section 8/9/28). Analyzes the grid and derives
//! probable UI structure. Every inferred object carries `confidence`, `evidence`,
//! and `source = "inferred"` so heuristic interpretation is never presented as
//! ground truth. When a framework adapter supplies native info, source = "native"
//! with confidence 1.0.

pub mod confidence;
pub mod controls;
pub mod focus;
pub mod recognizers;
pub mod regions;
pub mod relationships;

pub use confidence::Confidence;
pub use controls::{Control, ControlKind};
pub use focus::FocusInfo;
pub use regions::{ClippingState, Region, RegionKind};
pub use relationships::{Relationship, RelationshipEngine};

use crate::screen::ScreenState;

/// Full semantic analysis of one screen.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SemanticScreen {
    pub cols: u16,
    pub rows: u16,
    pub regions: Vec<Region>,
    pub controls: Vec<Control>,
    pub focus: FocusInfo,
    /// Spatial relationships between controls.
    pub relationships: Vec<(String, String, Relationship)>,
}

/// Build a semantic screen from a raw [`ScreenState`].
pub fn analyze(screen: &ScreenState) -> SemanticScreen {
    let regions = regions::detect_regions(screen);
    let controls = controls::detect_controls(screen, &regions);
    let focus = focus::infer_focus(screen, &controls);
    let relationships = RelationshipEngine::find_region_relationships(&controls, &regions);
    SemanticScreen {
        cols: screen.cols,
        rows: screen.rows,
        regions,
        controls,
        focus,
        relationships,
    }
}
