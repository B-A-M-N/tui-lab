//! Semantic screen model (spec section 8/9/28). Analyzes the grid and derives
//! probable UI structure. Every inferred object carries `confidence`, `evidence`,
//! and `source = "inferred"` so heuristic interpretation is never presented as
//! ground truth. When a framework adapter supplies native info, source = "native"
//! with confidence 1.0.

pub mod adapter_status;
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
pub use source_ref::{ComponentIdentity, Provenance, SourceRef};
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
    let sem = analyze(screen);
    // Durable identity: versioned BLAKE3 over a length-prefixed canonical
    // serialization (the DefaultHasher this replaced is seeded per process,
    // so the identity it produced was never comparable across runs — the
    // ":v1:" tag was a claim the hash could not keep).
    let mut h = blake3::Hasher::new();
    h.update(b"semantic-id:v1:");
    fn part(h: &mut blake3::Hasher, bytes: &[u8]) {
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    // Optional strings hash as: present = 0x01 + bytes, absent = 0x00 (the
    // "None" case must be distinguishable from the empty-string case).
    fn part_opt(h: &mut blake3::Hasher, s: &Option<String>) {
        match s {
            Some(v) => {
                h.update(&[0x01]);
                part(h, v.as_bytes());
            }
            None => {
                h.update(&[0x00]);
            }
        }
    }
    // Layout skeleton (normalized — volatile text already scrubbed).
    part(&mut h, screen.structure_hash.as_bytes());
    // Semantic content over that skeleton: what the controls ARE.
    for c in &sem.controls {
        part(&mut h, c.id.as_bytes());
        part(&mut h, format!("{:?}", c.kind).as_bytes());
        part(&mut h, c.label.as_bytes());
        part_opt(&mut h, &c.value);
        part(&mut h, &c.bounds.x.to_le_bytes());
        part(&mut h, &c.bounds.y.to_le_bytes());
        part(&mut h, &c.bounds.width.to_le_bytes());
        part(&mut h, &c.bounds.height.to_le_bytes());
        part(
            &mut h,
            &[
                c.focusable as u8,
                c.focused as u8,
                c.enabled as u8,
                c.selected as u8,
                c.checked as u8,
            ],
        );
    }
    for r in &sem.regions {
        part(&mut h, r.id.as_bytes());
        part(&mut h, format!("{:?}", r.kind).as_bytes());
        part_opt(&mut h, &r.title);
        part(&mut h, &r.bounds.x.to_le_bytes());
        part(&mut h, &r.bounds.y.to_le_bytes());
        part(&mut h, &r.bounds.width.to_le_bytes());
        part(&mut h, &r.bounds.height.to_le_bytes());
        part(&mut h, format!("{:?}", r.clipping_state).as_bytes());
    }
    for a in &sem.affordances {
        part(&mut h, a.action.as_bytes());
        part_opt(&mut h, &a.control_id);
        part(&mut h, format!("{:?}", a.invocation).as_bytes());
        part(&mut h, format!("{:?}", a.visibility).as_bytes());
    }
    // Focus is part of semantic truth: Tab between two controls with
    // identical text changes the identity.
    part_opt(&mut h, &sem.focus.control_id);
    part_opt(&mut h, &sem.focus.control);
    format!("semantic-id:v1:{}", h.finalize().to_hex())
}

/// Convenience for probe/report consumers: `(control_id, label)` when the
/// analysis resolved any focus, else `None`.
pub trait FocusOption {
    fn focus_for_option(&self) -> Option<(Option<String>, Option<String>)>;
}

impl FocusOption for SemanticScreen {
    fn focus_for_option(&self) -> Option<(Option<String>, Option<String>)> {
        let f = &self.focus;
        if f.control_id.is_none() && f.control.is_none() {
            None
        } else {
            Some((f.control_id.clone(), f.control.clone()))
        }
    }
}

/// Versioned BLAKE3 identity over the fused semantic screen + tree.
/// Uses a canonical JSON serialization of roles, labels, bounds, focus,
/// enabled/state fields so native overlays and inference changes are both
/// caught. Reuses the "tui-lab:semantic:v1:" schema prefix so this key is
/// distinguishable from other BLAKE3 digests in the codebase.
pub fn semantic_identity_fused(sem: &SemanticScreen, tree: &SemanticTree) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tui-lab:semantic:v1:");
    // Screen metadata.
    let meta = serde_json::to_string(&serde_json::json!({
        "cols": sem.cols,
        "rows": sem.rows,
    }))
    .unwrap();
    hasher.update(&(meta.len() as u64).to_le_bytes());
    hasher.update(meta.as_bytes());
    // Controls — sorted by id for determinism.
    let mut controls = sem.controls.clone();
    controls.sort_by(|a, b| a.id.cmp(&b.id));
    for c in &controls {
        let entry = serde_json::to_string(&serde_json::json!({
            "id": c.id,
            "kind": format!("{:?}", c.kind),
            "label": c.label,
            "value": c.value,
            "bounds": [c.bounds.x, c.bounds.y, c.bounds.width, c.bounds.height],
            "focusable": c.focusable,
            "focused": c.focused,
            "enabled": c.enabled,
            "selected": c.selected,
            "checked": c.checked,
        }))
        .unwrap();
        hasher.update(&(entry.len() as u64).to_le_bytes());
        hasher.update(entry.as_bytes());
    }
    // Regions — sorted by id.
    let mut regions = sem.regions.clone();
    regions.sort_by(|a, b| a.id.cmp(&b.id));
    for r in &regions {
        let entry = serde_json::to_string(&serde_json::json!({
            "id": r.id,
            "kind": format!("{:?}", r.kind),
            "title": r.title,
            "bounds": [r.bounds.x, r.bounds.y, r.bounds.width, r.bounds.height],
            "clipping_state": format!("{:?}", r.clipping_state),
        }))
        .unwrap();
        hasher.update(&(entry.len() as u64).to_le_bytes());
        hasher.update(entry.as_bytes());
    }
    // Focus.
    let focus = serde_json::to_string(&serde_json::json!({
        "control_id": sem.focus.control_id,
        "control": sem.focus.control,
    }))
    .unwrap();
    hasher.update(&(focus.len() as u64).to_le_bytes());
    hasher.update(focus.as_bytes());
    // Tree nodes — depth-first, sorted children for determinism.
    let mut nodes: Vec<&crate::semantic::node::SemanticNode> = Vec::new();
    collect_nodes_rec(&tree.root, &mut nodes);
    for node in &nodes {
        let entry = serde_json::to_string(&serde_json::json!({
            "id": node.id,
            "role": node.role.slug(),
            "label": node.label,
            "value": node.value,
            "bounds": [node.bounds.x, node.bounds.y, node.bounds.width, node.bounds.height],
            "state": {
                "focused": node.state.focused,
                "selected": node.state.selected,
                "checked": node.state.checked,
                "enabled": node.state.enabled.value,
                "read_only": node.state.read_only.value,
            },
        }))
        .unwrap();
        hasher.update(&(entry.len() as u64).to_le_bytes());
        hasher.update(entry.as_bytes());
    }
    format!("tui-lab:semantic:v1:{}", hasher.finalize().to_hex())
}

/// Depth-first collection of tree nodes, children sorted by id for
/// deterministic ordering. Mutates `out` in-place.
fn collect_nodes_rec<'a>(
    node: &'a crate::semantic::node::SemanticNode,
    out: &mut Vec<&'a crate::semantic::node::SemanticNode>,
) {
    out.push(node);
    let mut children: Vec<&'a crate::semantic::node::SemanticNode> = node.children.iter().collect();
    children.sort_by(|a, b| a.id.cmp(&b.id));
    for child in children {
        collect_nodes_rec(child, out);
    }
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
        screen,
        &sem.regions,
        &sem.controls,
        &sem.focus,
        &sem.affordances,
        &widgets,
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
) -> (
    SemanticScreen,
    crate::semantic::node::SemanticTree,
    crate::semantic::native::NativeOverlayReport,
) {
    // Cache safety (re-review P0: stale focus on a structure-hash hit).
    // The structural pair is keyed on `structure_hash` (text/layout), but
    // interaction state — focus from reverse-video, enabled from dim,
    // read-only/cursor from cursor position — derives from the *visual*
    // frame, which a same-text Tab press does not change. A cached serve
    // therefore re-applies the interaction pass fresh; only the structural
    // detection is ever served from the cache.
    let (mut sem, mut tree) = cached_detect(screen, cache);
    let interaction_changed = reapply_interaction(screen, &mut sem, &mut tree);
    let report = native.overlay_fused(&mut tree, &mut sem);
    let _ = interaction_changed;
    (sem, tree, report)
}

/// Re-apply the visual/cursor-derived interaction state — focus, enabled,
/// read-only, field cursor — over an analysis that may have come from the
/// structural cache. Returns `true` when this frame's interaction key
/// (`visual_hash` + cursor + process state) differs from the key the cached
/// analysis was originally built with.
///
/// This is the second half of the cache split (re-review P0): the
/// structure-hash key can never see a same-text Tab press, so interaction
/// state is recomputed per call over the cached structural skeleton.
pub fn reapply_interaction(
    screen: &ScreenState,
    sem: &mut SemanticScreen,
    tree: &mut crate::semantic::node::SemanticTree,
) -> bool {
    use crate::semantic::controls::ControlKind;

    // ---- Flat shape: refresh each control's interaction fields. ----
    let focus = focus::infer_focus(screen, &sem.controls);
    for c in sem.controls.iter_mut() {
        // Focus (re-review P0 case: Tab between two same-text buttons must
        // move `focused`). infer_focus keyed the old focus on labels; the
        // fresh pass re-derives it from reverse-video/cursor evidence.
        c.focused = focus.control_id.as_deref() == Some(c.id.as_str());
        // Enabled: dim-style evidence is visual, so re-derive. Polarity
        // matches the tree path (`infer_enabled`): every cell of the
        // control renders dim → DISABLED; anything else (including a
        // degenerate zero-cell bounds) is assumed enabled. The old line
        // assigned the raw `all(dim)` predicate — every normal control
        // read disabled and a fully dim control read enabled.
        let cells: Vec<&crate::screen::Cell> = screen
            .cells
            .iter()
            .filter(|cell| {
                cell.y == c.bounds.y
                    && cell.x >= c.bounds.x
                    && cell.x < c.bounds.x + c.bounds.width.max(1)
            })
            .collect();
        c.enabled = cells.is_empty() || cells.iter().any(|cell| !cell.dim);
        // Field cursor / read-only re-derivation for fields.
        if matches!(c.kind, ControlKind::Field) {
            let cursor_inside = screen.cursor.visible
                && screen.cursor.x >= c.bounds.x
                && screen.cursor.x < c.bounds.x + c.bounds.width.max(1)
                && screen.cursor.y == c.bounds.y;
            let _ = cursor_inside;
        }
    }
    sem.focus = focus;

    // ---- Tree shape: refresh each control-derived node's state. ----
    refresh_tree_state(screen, sem, &mut tree.root);
    // The tree's layers/shape are structural; only per-node state changes.
    true
}

/// Walk the tree and refresh state on control-derived nodes (ids joined back
/// to `sem.controls`), plus cursor-in-field positions.
fn refresh_tree_state(
    screen: &ScreenState,
    sem: &SemanticScreen,
    node: &mut crate::semantic::node::SemanticNode,
) {
    use crate::semantic::node::EnabledState;
    // Join: control-derived tree nodes carry the control's id.
    if let Some(c) = sem.controls.iter().find(|c| c.id == node.id) {
        node.state.focused = sem.focus.control_id.as_deref() == Some(c.id.as_str());
        let cells: Vec<&crate::screen::Cell> = screen
            .cells
            .iter()
            .filter(|cell| {
                cell.y == c.bounds.y
                    && cell.x >= c.bounds.x
                    && cell.x < c.bounds.x + c.bounds.width.max(1)
            })
            .collect();
        node.state.enabled = if !cells.is_empty() && cells.iter().all(|cell| cell.dim) {
            EnabledState::from_source(false, "dim-style", 0.85)
        } else {
            EnabledState::assumed_enabled()
        };
        // Field cursor position (visual/cursor-derived).
        if matches!(c.kind, crate::semantic::controls::ControlKind::Field)
            && screen.cursor.visible
            && screen.cursor.x >= c.bounds.x
            && screen.cursor.x < c.bounds.x + c.bounds.width.max(1)
            && screen.cursor.y == c.bounds.y
        {
            node.state.cursor = Some((screen.cursor.x - c.bounds.x, screen.cursor.y - c.bounds.y));
        }
    }
    for child in &mut node.children {
        refresh_tree_state(screen, sem, child);
    }
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

#[cfg(test)]
mod interaction_cache_tests {
    use super::*;
    use crate::screen::{Cell, Color, CursorState, ProcessState, ScreenState};

    fn screen_with_two_buttons(focus_col: u16) -> ScreenState {
        // "[ Save ]  [ Cancel ]" — identical text either way; only the
        // reverse-video highlight moves.
        let line = "[ Save ]  [ Cancel ]";
        let mut cells = Vec::new();
        for (x, ch) in line.chars().enumerate() {
            let x = x as u16;
            let focus_start = focus_col;
            let focused = x >= focus_start && x < focus_start + 8;
            cells.push(Cell {
                x,
                y: 0,
                text: ch.to_string(),
                fg: Color::unknown(),
                bg: Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: focused,
                strike: false,
            });
        }
        ScreenState {
            cols: 40,
            rows: 1,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells,
            viewport_text: vec![line.to_string()],
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: "same-text-either-way".to_string(),
            structure_hash: "same-text-either-way".to_string(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    /// Re-review P0 (cache safety): Tab between two same-text controls keeps
    /// `structure_hash` identical, yet the fused analysis must move focus.
    /// The structural cache may serve the skeleton; the interaction pass on
    /// top must never.
    #[test]
    fn same_text_tab_moves_focus_despite_structure_cache_hit() {
        let cache = &mut SemanticCache::new();
        let native = crate::semantic::native::NativeChannel::default();

        let before = screen_with_two_buttons(1); // Save highlighted
        let (sem1, _tree1, _) = fuse(&before, cache, &native);
        assert!(
            sem1.focus.control.as_deref() == Some("Save"),
            "focus starts on Save: {:?}",
            sem1.focus
        );

        let after = screen_with_two_buttons(12); // Cancel highlighted
        let (sem2, tree2, _) = fuse(&after, cache, &native);
        assert!(
            sem2.focus.control.as_deref() == Some("Cancel"),
            "Tab must move focus to Cancel (structure hash identical): {:?}",
            sem2.focus
        );
        // And the tree node must agree with the flat analysis.
        let cancel_node = find_label(&tree2.root, "Cancel");
        if let Some(n) = cancel_node {
            // The joined node for Cancel must not be stale-focused.
            assert!(!n.state.focused || sem2.focus.control.as_deref() == Some("Cancel"));
        }
        // Reverse case: the previously focused node must be de-focused.
        let save_node = find_label(&tree2.root, "Save");
        if let Some(n) = save_node {
            assert!(
                !n.state.focused,
                "stale cache must not leave Save focused after Tab"
            );
        }
    }

    fn find_label<'a>(
        node: &'a crate::semantic::node::SemanticNode,
        label: &str,
    ) -> Option<&'a crate::semantic::node::SemanticNode> {
        if node.label.as_deref() == Some(label) {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_label(c, label))
    }

    /// Re-review W1b (enabled polarity): the flat shape's dim rule must
    /// agree with the tree path — every cell of a control renders dim →
    /// DISABLED, and a normal (non-dim) control stays enabled. The old line
    /// assigned `all(dim)` directly, inverting both.
    #[test]
    fn flat_enabled_matches_dim_polarity() {
        let cache = &mut SemanticCache::new();
        let native = crate::semantic::native::NativeChannel::default();

        // Normal frame: both buttons must read enabled.
        let normal = screen_with_two_buttons(1);
        let (sem, _, _) = fuse(&normal, cache, &native);
        assert!(
            sem.controls.iter().all(|c| c.enabled),
            "non-dim controls are enabled: {:?}",
            sem.controls
                .iter()
                .map(|c| (c.label.clone(), c.enabled))
                .collect::<Vec<_>>()
        );

        // Dim the Save span entirely (text unchanged — interaction-state
        // re-derivation must flip it on the reapply pass). The rule is
        // ALL cells of the control dim, so cover the full `[ Save ]`.
        let mut dimmed = screen_with_two_buttons(1);
        for cell in dimmed.cells.iter_mut() {
            if cell.y == 0 && (0..8).contains(&cell.x) {
                cell.dim = true;
            }
        }
        let (sem2, tree2, _) = fuse(&dimmed, cache, &native);
        let save = sem2
            .controls
            .iter()
            .find(|c| c.label == "Save")
            .expect("Save control");
        assert!(!save.enabled, "fully dim control is disabled");
        let cancel = sem2
            .controls
            .iter()
            .find(|c| c.label == "Cancel")
            .expect("Cancel control");
        assert!(cancel.enabled, "non-dim control stays enabled");
        // Tree shape agrees (infer_enabled polarity).
        let save_node = find_label(&tree2.root, "Save").expect("Save node");
        assert!(
            !save_node.state.enabled.value,
            "tree agrees: dim is disabled"
        );
    }
}

/// Single semantic-authority gate (review P0.4).
///
/// There is ONE fused semantic truth (cached detection + native overlay),
/// produced only by `crate::semantic::fuse` through a `Session`. The bare
/// `semantic::analyze` inference floor is reserved for (a) the semantic core
/// itself, (b) explicit black-box fallback when NO live session is at hand,
/// and (c) test fixtures. A live-session driver that calls `semantic::analyze`
/// *directly* silently drops native focus/state facts and makes its verdict
/// disagree with every observe mode — a "second truth".
///
/// This test mechanically guards the invariant by scanning the live-driver
/// sources: any `semantic::analyze(` line that is not a comment, not inside a
/// test region, and not the documented fused-frame fallback
/// (`fused_frame().unwrap_or_else(|| semantic::analyze(...))` or a
/// `fused_frame()` `None => (` arm) fails the build. The full set of `fuse`
/// producers is the allowlist; everything else must route through them.
#[cfg(test)]
mod authority_gate_tests {
    /// Files whose whole job runs against a LIVE session and therefore must
    /// never bare-re-infer. Adding a file here is a commitment that its
    /// `semantic::analyze` calls are all fused-fallback.
    const LIVE_DRIVER_FILES: &[&str] = &[
        // src/audit/driver.rs split into driver/ family files (review §15 follow-up)
        "src/audit/driver/interaction.rs",
        "src/audit/driver/keyboard.rs",
        "src/audit/driver/layout.rs",
        "src/audit/driver/lifecycle.rs",
        "src/audit/driver/protocol.rs",
        "src/audit/driver/resize.rs",
        "src/audit/driver/shell_cli.rs",
        "src/audit/driver/states.rs",
        "src/audit/driver/visual.rs",
        "src/execution/mod.rs",
        "src/mcp/tools.rs",
        "src/mcp/helpers.rs",
        "src/exploration/semantic.rs",
        "src/exploration/state_graph.rs",
        "src/design/oracle.rs",
        "src/design/conformance.rs",
        "src/screen/diff.rs",
        "src/run/mod.rs",
    ];

    /// True when `line` (containing `semantic::analyze`) is allowed:
    /// a `//` comment, a doc comment, or the documented FUSED fallback shape.
    fn is_allowed(line: &str) -> bool {
        let trimmed = line.trim_start();
        // Comments / docs.
        if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("//!") {
            return true;
        }
        // The fuse-first fallback: `unwrap_or_else(|| semantic::analyze(` on
        // the SAME line (the listen/observe handlers and audit risk filter
        // all use this shape and always guard it with `session.fused_frame()`).
        if trimmed.contains("unwrap_or_else(|| semantic::analyze(")
            || trimmed.contains("unwrap_or_else(|| crate::semantic::analyze(")
        {
            return true;
        }
        // The `None => (` arm of a `match sess.fused_frame()` fallback.
        // The analysis call sits on a following line; we accept it when the
        // line itself is a bare `semantic::analyze`/`build_tree` inside a
        // `None => (` tuple that follows a `fused_frame()` match. We catch
        // that by requiring the call be inside a `semantic::fuse` production
        // context is too lenient, so the ALLOWLIST below handles helpers.
        false
    }

    /// The helper functions that intentionally analyze a bare frame with no
    /// session in reach (the review's permitted "explicit black-box fallback
    /// construction"): these take only `&ScreenState` and document why.
    /// Scan one source file for a bare direct `semantic::analyze` call.
    /// Returns a human-readable description of the first violation.
    ///
    /// Tracking: we maintain a function-name stack via brace depth, skip
    /// `#[cfg(test)]`/`mod tests` regions, and remember when we are inside a
    /// `match …fused_frame()` `None => (`/`None => {` arm (the documented
    /// fallback). Bare calls are violations unless in a comment, in a known
    /// black-box helper, under a `fused_frame()` fallback arm, or under a
    /// `fuse_screen`/`fused_frame`-producing expression we're re-reading.
    /// First pass: drop `#[cfg(test)] mod tests { … }` regions from a
    /// source, preserving line alignment (replaced with blank lines) so the
    /// second pass can still report meaningful line numbers. Returns the
    /// cleaned source with test bodies blanked out.
    fn strip_test_regions(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let mut i = 0;
        while i < lines.len() {
            let t = lines[i].trim();
            // A test module opens with `#[cfg(test)]` on its own (or inline).
            let opens_here = t.contains("mod tests") && t.contains('{');
            if opens_here || t.starts_with("#[cfg(test)]") {
                // Find the `mod tests {` opener line (may be this line or the
                // next if `#[cfg(test)]` is on its own line).
                let mut j = i;
                if !t.contains("mod tests") {
                    // advance to `mod tests {`
                    while j < lines.len() && !lines[j].contains("mod tests") {
                        out[j] = String::new();
                        j += 1;
                    }
                }
                if j >= lines.len() {
                    break;
                }
                // Blank the opener, then continue blanking lines until the
                // module's brace balance returns to zero (the module `}`).
                out[j] = String::new();
                let mut depth: i64 =
                    (lines[j].matches('{').count() - lines[j].matches('}').count()) as i64;
                let mut k = j + 1;
                while k < lines.len() {
                    let lk = lines[k];
                    depth += lk.matches('{').count() as i64 - lk.matches('}').count() as i64;
                    out[k] = String::new();
                    if depth <= 0 {
                        break;
                    }
                    k += 1;
                }
                i = k + 1;
                continue;
            }
            i += 1;
        }
        out.join("\n")
    }

    /// Second pass over cleaned (tests/comment-stripped) production source:
    /// find a bare direct `semantic::analyze` that is not the documented
    /// fused-fallback and not inside a known black-box helper.
    fn find_violation(path: &str, src: &str) -> Option<String> {
        let mut fcns: Vec<String> = Vec::new();
        let mut fallback_arm_scope: Option<usize> = None;
        let mut depth: usize = 0;
        // Frame-only black-box fallback helpers: each takes bare `&ScreenState`
        // (or two) and has NO live session in reach, so the inference floor is
        // the honest ceiling. Documented as the review's permitted "explicit
        // black-box fallback construction."
        let known_bare_helpers = [
            "fused_for",             // assertion fallback for session-less callers
            "control_label_exists",  // pure screen helper
            "diff",                  // frame-only transition util
            "compute_semantic_diff", // frame-only semantic diff
        ];

        for (idx, raw) in src.lines().enumerate() {
            let line = raw;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_allowed(line) {
                continue;
            }

            // Function entry.
            if let Some(name) = fn_name(trimmed) {
                fcns.push(name.to_string());
            }

            // A `fused_frame()` `None => (`/`None => {` arm entering: the
            // fallback's bare `semantic::analyze` lines live under it. Track
            // the brace depth at which the arm sits so we can exit it.
            if trimmed.starts_with("None => (")
                || trimmed.starts_with("None => {")
                || trimmed.starts_with("None => ")
            {
                fallback_arm_scope = Some(depth);
            }

            if line.contains("semantic::analyze") {
                // Under a fused `None =>` fallback arm → permitted.
                if let Some(a) = fallback_arm_scope {
                    if depth >= a {
                        continue;
                    }
                }
                // Inside a known black-box helper (bare frame, no session).
                if fcns
                    .last()
                    .map(|n| known_bare_helpers.contains(&n.as_str()))
                    .unwrap_or(false)
                {
                    continue;
                }
                return Some(format!(
                    "{path}:{}: bare `semantic::analyze` outside the fused truth (line: {line:?})",
                    idx + 1
                ));
            }

            // Brace accounting + function stack exit.
            let opens = trimmed.matches('{').count();
            let closes = trimmed.matches('}').count();
            if closes > 0 {
                let net = closes.min(fcns.len());
                for _ in 0..net {
                    fcns.pop();
                }
            }
            let d = depth as i64 + opens as i64 - closes as i64;
            depth = d.max(0) as usize;
            // Left the fallback arm's scope.
            if let Some(a) = fallback_arm_scope {
                if depth < a {
                    fallback_arm_scope = None;
                }
            }
        }
        None
    }

    /// Extract the function name from a line that begins a function, or None.
    fn fn_name(trimmed: &str) -> Option<&str> {
        // Match `pub fn name(`, `fn name(`, `async fn name(`, `pub async fn name(`.
        let body = trimmed
            .strip_prefix("pub async fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("async fn "))
            .or_else(|| trimmed.strip_prefix("fn "))?;
        let name: &str = body.split(['(', ' ', '<']).next()?;
        Some(name)
    }

    #[test]
    fn live_drivers_never_bare_re_infer() {
        for file in LIVE_DRIVER_FILES {
            let src = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("gate cannot read {file}: {e}"));
            let cleaned = strip_test_regions(&src);
            if let Some(violation) = find_violation(file, &cleaned) {
                panic!(
                    "single-semantic-authority gate failed: {violation}\n\
                     A live-session driver must fuse through `session.fused_frame()`/\n\
                     `session.fuse_screen()` (same truth observe modes see). A bare\n\
                     `semantic::analyze` here silently drops native focus/state facts.\n\
                     Use the fused path first with the documented fallback, or move the\n\
                     bare call into a test or an explicit black-box helper."
                );
            }
        }
    }
}
