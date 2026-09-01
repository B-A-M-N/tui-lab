//! SemanticNode tree builder (Wave C item 16).
//!
//! Merges regions, controls, and widget-family detections into one
//! [`SemanticTree`]. The old pipeline produced *parallel* structures —
//! regions with controls attached, components bolted on as opaque blobs —
//! and the consumer had to reconcile them. This builder is the single place
//! that knows how a `Region` becomes a `Dialog` node, a `Control` becomes a
//! `Button` node, and a detected widget becomes a subtree with its own
//! children (table → header → rows → cells).
//!
//! Layering (item 27) happens here too: top-level subtrees are tagged
//! [`Layer::Modal`] / [`Layer::Overlay`] / [`Layer::Status`] /
//! [`Layer::Background`] so consumers can ask "what's interactive *right
//! now*" without re-deriving which dialog owns the screen.

use std::collections::HashMap;

use crate::screen::ScreenState;
use crate::semantic::affordance::infer_affordances;
use crate::semantic::confidence::Confidence;
use crate::semantic::controls::{Control, ControlKind};
use crate::semantic::focus::FocusInfo;
use crate::semantic::node::{
    EnabledState, Layer, NodeState, Role, ScrollEdges, SemanticNode, SemanticTree,
};
use crate::semantic::regions::{Bounds, Region, RegionKind};
use crate::semantic::widgets;

/// Build the full semantic tree from a screen.
pub fn build_tree(screen: &ScreenState) -> SemanticTree {
    let regions = crate::semantic::regions::detect_regions(screen);
    let controls = crate::semantic::controls::detect_controls(screen, &regions);
    let focus = crate::semantic::focus::infer_focus(screen, &controls);
    let affordances = infer_affordances(screen, &controls);
    let widgets = widgets::detect_widgets(screen, &regions);
    build_tree_from_parts(screen, &regions, &controls, &focus, &affordances, &widgets)
}

/// Assemble the tree from pre-computed parts (tests and internal callers).
pub fn build_tree_from_parts(
    screen: &ScreenState,
    regions: &[Region],
    controls: &[Control],
    focus: &FocusInfo,
    affordances: &[crate::semantic::affordance::Affordance],
    widgets: &[widgets::Widget],
) -> SemanticTree {
    let mut root = SemanticNode {
        id: "screen".to_string(),
        role: Role::Screen,
        parent: None,
        bounds: Bounds {
            x: 0,
            y: 0,
            width: screen.cols,
            height: screen.rows,
        },
        label: screen.title.clone(),
        value: None,
        state: NodeState::default(),
        children: Vec::new(),
        affordances: Vec::new(),
        identity: None,
        confidence: Confidence::inferred(1.0, &["grid"]),
    };

    // Widget subtrees first: their internal nodes claim cells; region and
    // control placement then works around them.
    let mut widget_nodes: Vec<SemanticNode> = Vec::new();
    for w in widgets {
        widget_nodes.push(widgets::widget_to_node(w, screen));
    }

    // Region → container nodes.
    let mut region_nodes: HashMap<String, SemanticNode> = regions
        .iter()
        .map(|r| (r.id.clone(), region_to_node(r)))
        .collect();

    // Nest widget subtrees into their smallest containing region (or root).
    for wn in widget_nodes {
        attach_to_smallest(&mut region_nodes, &mut root, wn);
    }

    // Controls → leaf nodes, attached to their region (already bound during
    // detect_controls) or the root.
    for c in controls {
        let n = control_to_node(c, focus, affordances, screen);
        match c
            .region_id
            .as_deref()
            .and_then(|rid| region_nodes.get_mut(rid))
        {
            Some(rn) => rn.children.push(n),
            None => root.children.push(n),
        }
    }

    // Nest regions under their parents (same order-stable scheme as the
    // legacy state tree builder).
    nest_regions(&mut region_nodes, regions, &mut root);

    // OSC8 hyperlinks (item 29): leaf nodes anchored at their span.
    for link in &screen.hyperlinks {
        let n = hyperlink_to_node(link);
        attach_to_smallest(&mut region_nodes, &mut root, n);
    }

    // Layer tags for top-level subtrees (item 27).
    let mut layers = HashMap::new();
    assign_layers(&root, &mut layers);

    SemanticTree { root, layers }
}

/// Item 29: an OSC8 hyperlink as a node. The label is the link *text* (the
/// cells under the span), the value carries the URI — observation only.
fn hyperlink_to_node(link: &crate::screen::cell::Hyperlink) -> SemanticNode {
    let (x0, y0) = link.start;
    let (x1, y1) = link.end.unwrap_or(link.start);
    // Span width: same-row links are common; multi-row spans take the first
    // row's extent (a hyperlink node is a pointer, not a text container).
    let width = if y1 == y0 {
        x1.saturating_sub(x0).max(1)
    } else {
        1
    };
    SemanticNode {
        id: format!("link/{}/{}", link.id.clone().unwrap_or_default(), x0),
        role: Role::Hyperlink,
        parent: None,
        bounds: Bounds {
            x: x0,
            y: y0,
            width,
            height: 1,
        },
        label: None,
        value: Some(link.uri.clone()),
        state: NodeState::default(),
        children: Vec::new(),
        affordances: Vec::new(),
        identity: None,
        confidence: Confidence::native(),
    }
}

/// Convert a region to a container node.
fn region_to_node(r: &Region) -> SemanticNode {
    SemanticNode {
        id: r.id.clone(),
        role: region_role(&r.kind),
        parent: r.parent_id.clone(),
        bounds: r.bounds.clone(),
        label: r.title.clone(),
        value: None,
        state: NodeState::default(),
        children: Vec::new(),
        affordances: Vec::new(),
        identity: None,
        confidence: r.confidence.clone(),
    }
}

fn region_role(kind: &RegionKind) -> Role {
    match kind {
        RegionKind::Dialog => Role::Dialog,
        RegionKind::Panel => Role::Panel,
        RegionKind::Toolbar => Role::Toolbar,
        RegionKind::Footer => Role::Footer,
        RegionKind::List => Role::List,
        RegionKind::Table => Role::Table,
        RegionKind::Unknown => Role::Unknown,
    }
}

/// Convert a control to a leaf node, enriching state with focus and enabled
/// provenance (items 28).
fn control_to_node(
    c: &Control,
    focus: &FocusInfo,
    affordances: &[crate::semantic::affordance::Affordance],
    screen: &ScreenState,
) -> SemanticNode {
    let focused = focus.control_id.as_deref() == Some(c.id.as_str());
    let enabled = infer_enabled(c, screen);
    let read_only = infer_read_only(c, screen);
    let mut state = NodeState {
        focusable: c.focusable,
        focused,
        selected: c.selected,
        checked: c.checked,
        enabled,
        read_only,
        ..NodeState::default()
    };
    if matches!(c.kind, ControlKind::Field) {
        if let Some((cx, cy)) = field_cursor(c, screen) {
            state.cursor = Some((cx, cy));
        }
    }
    let role = control_role(&c.kind);
    let own_affordances: Vec<_> = affordances
        .iter()
        .filter(|a| a.control_id.as_deref() == Some(c.id.as_str()))
        .cloned()
        .collect();
    SemanticNode {
        id: c.id.clone(),
        role,
        parent: c.region_id.clone(),
        bounds: Bounds {
            x: c.bounds.x,
            y: c.bounds.y,
            width: c.bounds.width,
            height: c.bounds.height,
        },
        label: Some(c.label.clone()),
        value: c.value.clone(),
        state,
        children: Vec::new(),
        affordances: own_affordances,
        identity: None,
        confidence: c.confidence.clone(),
    }
}

fn control_role(kind: &ControlKind) -> Role {
    match kind {
        ControlKind::Button => Role::Button,
        ControlKind::Field => Role::Field,
        ControlKind::Checkbox => Role::Checkbox,
        ControlKind::Radio => Role::Radio,
        ControlKind::Tab => Role::Tab,
        ControlKind::List => Role::List,
        ControlKind::MenuItem => Role::MenuItem,
        ControlKind::Status => Role::Status,
        ControlKind::Progress => Role::Progress,
        ControlKind::Spinner => Role::Spinner,
        ControlKind::Label => Role::Label,
        ControlKind::Unknown => Role::Unknown,
    }
}

/// Item 28: enabled inference with provenance.
///
/// Signals, strongest first:
///   1. every cell of the control renders dim → disabled (`dim-style`);
///   2. framework adapter says so (`adapter`, confidence 1.0) — not yet
///      available for inferred controls, kept for the native path;
///   3. otherwise: enabled assumed (`default`).
fn infer_enabled(c: &Control, screen: &ScreenState) -> EnabledState {
    let cells: Vec<&crate::screen::Cell> = screen
        .cells
        .iter()
        .filter(|cell| {
            cell.y == c.bounds.y
                && cell.x >= c.bounds.x
                && cell.x < c.bounds.x + c.bounds.width.max(1)
        })
        .collect();
    if !cells.is_empty() && cells.iter().all(|cell| cell.dim) {
        return EnabledState::from_source(false, "dim-style", 0.85);
    }
    EnabledState::assumed_enabled()
}

/// Item 28: read-only inference. A field whose cells never change and whose
/// cursor never enters it is *plausibly* read-only, but a single frame cannot
/// prove it; we only claim what one frame shows — text present, no cursor.
fn infer_read_only(c: &Control, screen: &ScreenState) -> crate::semantic::node::ReadOnlyState {
    use crate::semantic::node::ReadOnlyState;
    if c.kind != ControlKind::Field {
        return ReadOnlyState::default();
    }
    let cursor_inside = screen.cursor.x >= c.bounds.x
        && screen.cursor.x < c.bounds.x + c.bounds.width.max(1)
        && screen.cursor.y == c.bounds.y;
    if c.value.as_deref().is_some_and(|v| !v.is_empty()) && !cursor_inside {
        return ReadOnlyState {
            value: true,
            source: "no-cursor-in-field".to_string(),
            confidence: 0.4,
        };
    }
    ReadOnlyState::default()
}

/// Field cursor position, relative to the field's bounds.
fn field_cursor(c: &Control, screen: &ScreenState) -> Option<(u16, u16)> {
    if !screen.cursor.visible {
        return None;
    }
    let inside = screen.cursor.x >= c.bounds.x
        && screen.cursor.x < c.bounds.x + c.bounds.width.max(1)
        && screen.cursor.y == c.bounds.y;
    inside.then_some((screen.cursor.x - c.bounds.x, screen.cursor.y - c.bounds.y))
}

/// Attach a node to the smallest region containing its midpoint, else root.
fn attach_to_smallest(
    region_nodes: &mut HashMap<String, SemanticNode>,
    root: &mut SemanticNode,
    node: SemanticNode,
) {
    let b = &node.bounds;
    let (mx, my) = (b.x + b.width / 2, b.y + b.height / 2);
    let mut best: Option<(String, u32)> = None;
    for (id, rn) in region_nodes.iter() {
        let nb = &rn.bounds;
        if mx >= nb.x && my >= nb.y && mx < nb.x + nb.width && my < nb.y + nb.height {
            let area = nb.width as u32 * nb.height as u32;
            if best.as_ref().map(|(_, a)| area < *a).unwrap_or(true) {
                best = Some((id.clone(), area));
            }
        }
    }
    match best {
        Some((id, _)) => {
            let mut node = node;
            node.parent = Some(id.clone());
            if let Some(target) = region_nodes.get_mut(&id) {
                target.children.push(node);
            } else {
                root.children.push(node);
            }
        }
        None => root.children.push(node),
    }
}

/// Nest region nodes under their parents (regions arrive in arbitrary
/// order; take any whose parent is materialized and repeat).
fn nest_regions(
    region_nodes: &mut HashMap<String, SemanticNode>,
    regions: &[Region],
    root: &mut SemanticNode,
) {
    let parents: HashMap<String, Option<String>> = regions
        .iter()
        .map(|r| (r.id.clone(), r.parent_id.clone()))
        .collect();

    let mut pending: Vec<String> = regions.iter().map(|r| r.id.clone()).collect();
    loop {
        let mut placed = 0;
        let mut still: Vec<String> = Vec::new();
        for id in &pending {
            let parent = parents
                .get(id)
                .cloned()
                .flatten()
                .filter(|pid| pid == "screen" || region_nodes.contains_key(pid));
            match parent {
                Some(pid) if pid != "screen" => {
                    if let Some(mut node) = region_nodes.remove(id) {
                        node.parent = Some(pid.clone());
                        if let Some(p) = region_nodes.get_mut(&pid) {
                            p.children.push(node);
                            placed += 1;
                        } else {
                            region_nodes.insert(id.clone(), node);
                            still.push(id.clone());
                        }
                    }
                }
                _ => {
                    if let Some(mut node) = region_nodes.remove(id) {
                        node.parent = None;
                        root.children.push(node);
                        placed += 1;
                    }
                }
            }
        }
        if placed == 0 || still.is_empty() && region_nodes.is_empty() {
            // Anything left cannot be placed (shouldn't happen; parents are
            // acyclic) — attach to the root so no region is lost.
            for (_, mut node) in region_nodes.drain() {
                node.parent = None;
                root.children.push(node);
            }
            break;
        }
        pending = still;
        if region_nodes.is_empty() {
            break;
        }
    }
}

/// Item 27: assign a layer to each top-level subtree.
///
/// Heuristics, in priority order:
///   * a Dialog region (bordered, titled, smaller than the viewport) → Modal;
///   * toasts/alerts/palettes/dropdowns → Overlay;
///   * toolbars/footers/key-hint bars → Status;
///   * everything else → Background.
fn assign_layers(root: &SemanticNode, layers: &mut HashMap<String, Layer>) {
    for child in &root.children {
        let layer = match child.role {
            Role::Dialog => Layer::Modal,
            Role::Toast | Role::Alert | Role::CommandPalette | Role::Dropdown | Role::Overlay => {
                Layer::Overlay
            }
            Role::Toolbar
            | Role::Footer
            | Role::Status
            | Role::StatusLayer
            | Role::KeyHint
            | Role::HelpOverlay => Layer::Status,
            _ => {
                // A titled bordered box that is not chrome reads as a modal
                // even when the region classifier (which keys on nesting)
                // called it a panel: modality is about *occupying* the
                // screen's attention, and a lone titled box does that.
                if is_hint_bar(child) {
                    Layer::Status
                } else if is_modal_box(child, root.bounds.width, root.bounds.height) {
                    Layer::Modal
                } else {
                    Layer::Background
                }
            }
        };
        layers.insert(child.id.clone(), layer);
    }
}

/// A bordered container that reads as a modal: titled (or dialog-classed),
/// covering a minority of the screen, not full-viewport chrome. Regions
/// exist only where a border was traced, so containment implies a border.
/// Leaf controls never qualify — a button with a label is not a modal.
fn is_modal_box(n: &SemanticNode, cols: u16, rows: u16) -> bool {
    if !matches!(
        n.role,
        Role::Dialog | Role::Panel | Role::Unknown | Role::Overlay
    ) {
        return false;
    }
    let b = &n.bounds;
    let covers_screen = b.width >= cols.saturating_sub(1) && b.height >= rows.saturating_sub(1);
    let small_enough = (b.width as u32 * b.height as u32) * 2 < cols as u32 * rows as u32;
    let titled_or_dialog = n.role == Role::Dialog || n.label.is_some();
    let not_chrome = !matches!(n.role, Role::Toolbar | Role::Footer);
    titled_or_dialog && not_chrome && !covers_screen && small_enough
}

/// A one-row bottom/top bar whose text is mostly key hints ("q quit  ^X exit").
fn is_hint_bar(n: &SemanticNode) -> bool {
    if n.bounds.height != 1 {
        return false;
    }
    let Some(label) = &n.label else { return false };
    let words: Vec<&str> = label.split_whitespace().collect();
    if words.len() < 2 {
        return false;
    }
    // Heuristic: several short tokens, half of them 1-2 chars (keys).
    let keyish = words.iter().filter(|w| w.chars().count() <= 3).count();
    keyish * 2 >= words.len()
}

// ─── Scroll edges helper shared with widgets ─────────────────────────────

/// Compute scroll edges from a scrollbar's thumb geometry.
///
/// A thumb at the top of its track means no content above; at the bottom,
/// no content below. Track length 0 → no claims.
pub fn scroll_edges_from_thumb(thumb_offset: u16, thumb_len: u16, track_len: u16) -> ScrollEdges {
    let mut e = ScrollEdges {
        up: false,
        down: false,
        left: false,
        right: false,
    };
    if track_len == 0 {
        return e;
    }
    e.up = thumb_offset > 0;
    e.down = thumb_offset + thumb_len < track_len;
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CursorState, ProcessState};

    pub(crate) fn screen(rows: Vec<&str>, cols: u16) -> ScreenState {
        ScreenState {
            cols,
            rows: rows.len() as u16,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn screen_becomes_root_with_controls() {
        let s = screen(vec!["[ Save ]  [ Cancel ]"], 40);
        let tree = build_tree(&s);
        assert_eq!(tree.root.role, Role::Screen);
        let buttons: Vec<_> = tree
            .root
            .children
            .iter()
            .filter(|c| c.role == Role::Button)
            .collect();
        assert_eq!(buttons.len(), 2, "{:?}", tree.render());
    }

    /// Regions become container nodes; controls nest inside them. The box
    /// must sit mid-screen (a box at y=0 with height<=4 is a toolbar by the
    /// region classifier's own rules — the fixture reflects a real modal).
    #[test]
    fn dialog_region_becomes_modal_layer() {
        let mut rows: Vec<String> = vec![String::new(); 10];
        rows[2] = "┌─ Settings ───────────────┐".into();
        rows[3] = "│ Host: localhost          │".into();
        rows[4] = "│ [ Save ]                 │".into();
        rows[5] = "└──────────────────────────┘".into();
        let s = screen(rows.iter().map(|r| r.as_str()).collect(), 40);
        let tree = build_tree(&s);
        assert!(
            tree.layers.values().any(|l| *l == Layer::Modal),
            "dialog must tag Modal: {:?}",
            tree.layers
        );
        let dialog = tree
            .root
            .children
            .iter()
            .find(|c| tree.layers.get(&c.id) == Some(&Layer::Modal))
            .expect("dialog node");
        assert_eq!(dialog.label.as_deref(), Some("Settings"));
        assert!(
            dialog
                .children
                .iter()
                .any(|c| c.role == Role::Button && c.label.as_deref() == Some("Save")),
            "Save nests in the dialog: {}",
            tree.render()
        );
    }

    /// Item 28: enabled carries provenance; plain controls stay "default".

    #[test]
    fn enabled_state_has_provenance() {
        let s = screen(vec!["[ Save ]"], 40);
        let tree = build_tree(&s);
        let btn = tree
            .root
            .children
            .iter()
            .find(|c| c.role == Role::Button)
            .unwrap();
        assert!(btn.state.enabled.value);
        assert_eq!(btn.state.enabled.source, "default");
    }

    /// Focus flows from FocusInfo into node state.
    #[test]
    fn focused_control_marked() {
        let s = screen(vec!["[ Save ]  [ Cancel ]"], 40);
        let controls = crate::semantic::controls::detect_controls(&s, &[]);
        let save = controls.iter().find(|c| c.label == "Save").unwrap();
        let focus = FocusInfo {
            control: Some("Save".into()),
            control_id: Some(save.id.clone()),
            confidence: 0.94,
            evidence: vec!["test".into()],
        };
        let tree = build_tree_from_parts(&s, &[], &controls, &focus, &[], &[]);
        let n = tree.root.find(&save.id).unwrap();
        assert!(n.state.focused, "focus must land on the Save node");
    }

    /// Item 27: a footer hint bar is Status layer, not Background.
    #[test]
    fn hint_bar_is_status_layer() {
        let s = screen(vec!["body content here", "q quit  ^X exit  [F1] Help"], 40);
        let tree = build_tree(&s);
        let status = tree.layer_nodes(Layer::Status);
        assert!(!status.is_empty(), "layers: {:?}", tree.layers);
    }

    /// Item 29, end to end: an OSC8 link in ScreenState becomes a Hyperlink
    /// node in the tree, value = URI (observation only). Item 28: a control
    /// rendered all-dim infers disabled with dim-style
    /// provenance, not the default assumption.
    #[test]
    fn dim_control_infers_disabled() {
        use crate::screen::Cell;
        let mut s = screen(vec!["[ Save ]"], 40);
        // Fill cells for the Save button row (x 0..8) with dim attribute.
        s.cells = (0..40)
            .map(|x| Cell {
                x,
                y: 0,
                text: if x < 8 {
                    s.viewport_text[0]
                        .chars()
                        .nth(x as usize)
                        .map(|c| c.to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                },
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: x < 8,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            })
            .collect();
        let tree = build_tree(&s);
        let btn = tree
            .root
            .children
            .iter()
            .find(|c| c.role == Role::Button && c.label.as_deref() == Some("Save"))
            .expect("button");
        assert!(!btn.state.enabled.value, "dim row is disabled");
        assert_eq!(btn.state.enabled.source, "dim-style");
        assert!(btn.state.enabled.confidence > 0.5);
    }

    #[test]
    fn hyperlink_becomes_node_with_uri() {
        let mut s = screen(vec!["see docs for more"], 40);
        s.hyperlinks.push(crate::screen::cell::Hyperlink {
            id: Some("docs-1".into()),
            uri: "https://example.com/docs".into(),
            start: (4, 0),
            end: Some((8, 0)),
        });
        let tree = build_tree(&s);
        let link = tree
            .root
            .children
            .iter()
            .find(|c| c.role == Role::Hyperlink)
            .expect("hyperlink node");
        assert_eq!(link.value.as_deref(), Some("https://example.com/docs"));
        assert_eq!(link.bounds.x, 4);
        assert_eq!(link.bounds.width, 4);
        assert_eq!(link.confidence.source, "native", "OSC8 is observed fact");
    }

    #[test]
    fn scroll_edges_from_thumb_geometry() {
        // Thumb at top: can't scroll up, can scroll down.
        let e = scroll_edges_from_thumb(0, 3, 10);
        assert!(!e.up && e.down);
        assert!(e.at_start());
        // Thumb at bottom: at end.
        let e = scroll_edges_from_thumb(7, 3, 10);
        assert!(e.up && !e.down);
        assert!(e.at_end());
        // Full-track thumb: nowhere to go.
        let e = scroll_edges_from_thumb(0, 10, 10);
        assert!(e.at_start() && e.at_end());
    }
}
