//! Semantic screen model (spec section 8/9/28). Analyzes the grid and derives
//! probable UI structure. Every inferred object carries `confidence`, `evidence`,
//! and `source = "inferred"` so heuristic interpretation is never presented as
//! ground truth. When a framework adapter supplies native info, source = "native"
//! with confidence 1.0.

pub mod affordance;
pub mod cache;
pub mod components;
pub mod confidence;
pub mod controls;
pub mod focus;
pub mod focus_graph;
pub mod invoke_affordance;
pub mod native;
pub mod node;
pub mod recognizers;
pub mod regions;
pub mod relationships;
pub mod source_ref;
pub mod state_tree;
pub mod tree_builder;
pub mod widgets;

pub use affordance::{infer_affordances, Affordance, Invocation, Visibility};
pub use cache::{CacheResult, SemanticCache, MAX_ENTRIES};

pub use components::{
    detect_components, Component, ScrollbarComponent, ScrollbarOrientation, TableComponent,
    TreeComponent,
};
pub use confidence::Confidence;
pub use controls::{Control, ControlKind};
pub use focus::FocusInfo;
pub use focus_graph::{FocusEdge, FocusGraph, FocusNode, TransitionSource};
pub use node::{
    EnabledState, Layer, NodeState, ReadOnlyState, Role, ScrollEdges, SemanticNode, SemanticTree,
};
pub use regions::{ClippingState, Region, RegionKind};
pub use relationships::{Relationship, RelationshipEngine, SemanticRelation};
pub use state_tree::{build_state_tree, ControlEntry, StateNode, TerminalStateTree};
pub use tree_builder::build_tree;
pub use widgets::{detect_widgets, Widget, WidgetKind};

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

/// Re-review P0 (real SemanticChange): a stable identity over the semantic
/// content of a screen — which controls exist, their roles/labels/bounds,
/// the region layout, affordances, and focus. Two frames with the same
/// identity are semantically identical *even if* pixels differ (spinner
/// frame, clock, cursor blink), and two frames with different identities
/// are a genuine semantic change even if the structure hash alone would
/// not move.
///
/// Deliberately NOT cached: completion evaluation calls this rarely (once
/// per action on the before-frame, then on candidate after-frames), and
/// caching risks exactly the staleness this exists to prevent.
pub fn semantic_identity(screen: &ScreenState) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let sem = analyze(screen);
    let mut h = DefaultHasher::new();
    // Layout skeleton (normalized — volatile text already scrubbed).
    screen.structure_hash.hash(&mut h);
    // Semantic content over that skeleton: what the controls ARE.
    for c in &sem.controls {
        c.id.hash(&mut h);
        format!("{:?}", c.kind).hash(&mut h);
        c.label.hash(&mut h);
        c.value.hash(&mut h);
        c.bounds.x.hash(&mut h);
        c.bounds.y.hash(&mut h);
        c.bounds.width.hash(&mut h);
        c.bounds.height.hash(&mut h);
        c.focusable.hash(&mut h);
        c.focused.hash(&mut h);
        c.enabled.hash(&mut h);
        c.selected.hash(&mut h);
        c.checked.hash(&mut h);
    }
    for r in &sem.regions {
        r.id.hash(&mut h);
        format!("{:?}", r.kind).hash(&mut h);
        r.title.hash(&mut h);
        r.bounds.x.hash(&mut h);
        r.bounds.y.hash(&mut h);
        r.bounds.width.hash(&mut h);
        r.bounds.height.hash(&mut h);
        format!("{:?}", r.clipping_state).hash(&mut h);
    }
    for a in &sem.affordances {
        a.action.hash(&mut h);
        a.control_id.hash(&mut h);
        format!("{:?}", a.invocation).hash(&mut h);
        format!("{:?}", a.visibility).hash(&mut h);
    }
    // Focus is part of semantic truth: Tab between two controls with
    // identical text changes the identity.
    sem.focus.control_id.hash(&mut h);
    sem.focus.control.hash(&mut h);
    format!("semantic-id:v1:{:016x}", h.finish())
}

/// One detection pass producing BOTH shapes — the flat
/// [`SemanticScreen`] and the [`SemanticTree`] — from the same detector
/// outputs. This is the structural guarantee behind fused semantic truth
/// (re-review Wave-4): observe modes that render different shapes can never
/// disagree about what's on screen, because they are built from one
/// analysis, not two.
pub fn detect_frame(screen: &ScreenState) -> (SemanticScreen, crate::semantic::node::SemanticTree) {
    let sem = analyze(screen);
    let widgets = widgets::detect_widgets(screen, &sem.regions);
    let tree = tree_builder::build_tree_from_parts(
        screen, &sem.regions, &sem.controls, &sem.focus, &sem.affordances, &widgets,
    );
    (sem, tree)
}

/// The fused truth for a screen: detection cached on `structure_hash`, then
/// the native overlay applied fresh on every call. This is the one path all
/// semantic-bearing observe modes route through.
///
/// The overlay is deliberately *outside* the cache: an app can rewrite its
/// self-report without changing screen structure (focus moved, a value
/// changed) — that is exactly the case the cache must not freeze.
pub fn fuse(
    screen: &ScreenState,
    cache: &mut SemanticCache,
    native: &crate::semantic::native::NativeChannel,
) -> (SemanticScreen, crate::semantic::node::SemanticTree, crate::semantic::native::NativeOverlayReport) {
    let (sem, tree) = cached_detect(screen, cache);
    let mut sem = sem;
    let mut tree = tree;
    let report = native.overlay_fused(&mut tree, &mut sem);
    (sem, tree, report)
}

/// Cached detection of both shapes: hit → clone from cache; miss → run
/// [`detect_frame`] once and store both.
fn cached_detect(
    screen: &ScreenState,
    cache: &mut SemanticCache,
) -> (SemanticScreen, crate::semantic::node::SemanticTree) {
    let key = screen.structure_hash.clone();
    if key.is_empty() {
        return detect_frame(screen);
    }
    if let Some(entry) = cache.entries.iter().position(|(k, _)| *k == key) {
        let (sem, tree) = cache.entries[entry].1.clone();
        return (sem, tree);
    }
    let pair = detect_frame(screen);
    cache.insert(key, pair.clone());
    pair
}

/// The tree from a fused (native-overlaid) frame, for callers that only
/// need the node shape.
pub fn fuse_tree(
    screen: &ScreenState,
    cache: &mut SemanticCache,
    native: &crate::semantic::native::NativeChannel,
) -> crate::semantic::node::SemanticTree {
    fuse(screen, cache, native).1
}
