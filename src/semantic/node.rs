//! The general semantic node (Wave C items 16 + 27).
//!
//! Everything on a screen is a node: the screen itself, regions, controls,
//! widget internals (table headers/rows/cells, tree items, scroll regions),
//! hyperlinks, help hints. A node carries a uniform envelope —
//! `{id, role, parent, children, bounds, label, value, state, affordances,
//! source, confidence, evidence}` — so an agent can walk one tree and reason
//! about "a table with a focused row" instead of a container with an opaque
//! `Component::Table` attachment.
//!
//! Layering (item 27): a screen frequently has a *background* UI plus a
//! modal *dialog* plus a *status layer* (toasts, key-hint bars). The builder
//! tags each top-level subtree with a [`Layer`], so "what's interactive"
//! can be answered layer-first: the dialog's controls matter, the dimmed
//! background's do not.
//!
//! Provenance (item 28): `state.enabled` is an [`EnabledState`], not a bare
//! `bool` — `"default"`, i.e. "we assumed enabled because nothing said
//! otherwise", is a different claim than `"dim-style"`, "we saw the dim
//! attribute on every cell of this control".

use crate::semantic::affordance::Affordance;
use crate::semantic::confidence::Confidence;
use crate::semantic::regions::Bounds;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The role a node plays in the UI tree.
///
/// Region-derived roles, control kinds, and widget-internal parts all live
/// here so the tree is one type, not a container enum with attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    // Structure
    Screen,
    Dialog,
    Panel,
    Toolbar,
    Footer,
    StatusLayer,
    /// An overlay that does not take exclusive control (dropdown, tooltip).
    Overlay,
    // Widgets
    Table,
    TableHeader,
    TableRow,
    TableCell,
    Tree,
    TreeItem,
    ScrollableRegion,
    List,
    ListItem,
    TextArea,
    Select,
    Dropdown,
    Menu,
    MenuItem,
    CommandPalette,
    SplitPane,
    Toast,
    Alert,
    Validation,
    // Controls (mirror ControlKind)
    Button,
    Field,
    Checkbox,
    Radio,
    Tab,
    Status,
    Progress,
    Spinner,
    Label,
    // Item 29
    Hyperlink,
    HelpOverlay,
    KeyHint,
    Unknown,
}

impl Role {
    /// The kebab-case slug used in node IDs and text renders.
    pub fn slug(&self) -> String {
        let s = format!("{:?}", self).to_lowercase();
        s
    }

    /// The unified control projection (re-review item 36): every Role
    /// projects onto exactly one `ControlKind` — or `None` for roles
    /// that are structure/annotation, not interactive controls. This is
    /// the ONE mapping; `kind_from_slug` on the native path, the intent
    /// resolver's `kind_role_slug`, and any future flat-shape consumer
    /// derive from here so a role can never disagree with its own
    /// control classification.
    pub fn control_kind(&self) -> Option<crate::semantic::controls::ControlKind> {
        use crate::semantic::controls::ControlKind as K;
        Some(match self {
            Role::Button => K::Button,
            Role::Field | Role::TextArea => K::Field,
            Role::Checkbox => K::Checkbox,
            Role::Radio => K::Radio,
            Role::Tab => K::Tab,
            Role::List | Role::ListItem | Role::Select | Role::Dropdown => K::List,
            Role::Menu | Role::MenuItem => K::MenuItem,
            Role::Status => K::Status,
            Role::Progress => K::Progress,
            Role::Spinner => K::Spinner,
            Role::Label => K::Label,
            // Structure and annotation roles are not controls.
            Role::Screen
            | Role::Dialog
            | Role::Panel
            | Role::Toolbar
            | Role::Footer
            | Role::StatusLayer
            | Role::Overlay
            | Role::Table
            | Role::TableHeader
            | Role::TableRow
            | Role::TableCell
            | Role::Tree
            | Role::TreeItem
            | Role::ScrollableRegion
            | Role::CommandPalette
            | Role::SplitPane
            | Role::Toast
            | Role::Alert
            | Role::Validation
            | Role::Hyperlink
            | Role::HelpOverlay
            | Role::KeyHint
            | Role::Unknown => return None,
        })
    }
}

/// Layer assignment (item 27): which interaction plane a subtree occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// The base application UI.
    Background,
    /// A modal dialog: takes exclusive input; the background is inert.
    Modal,
    /// A non-modal floating layer (toast, palette, dropdown).
    Overlay,
    /// Status chrome: key-hint bars, status lines — visible, rarely focused.
    Status,
}

/// Provenance-carrying enabled flag (item 28).
///
/// `enabled: true` was a *default assumption*, not evidence. This type makes
/// the distinction a first-class part of every node's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnabledState {
    pub value: bool,
    /// How we know: `"default"` (assumed), `"dim-style"`, `"glyph"`,
    /// `"adapter"` (framework), `"probe"` (interaction evidence).
    pub source: String,
    /// How much to believe the inference (1.0 for adapters).
    pub confidence: f32,
}

impl Default for EnabledState {
    fn default() -> Self {
        EnabledState {
            value: true,
            source: "default".to_string(),
            confidence: 0.5,
        }
    }
}

impl EnabledState {
    pub fn assumed_enabled() -> Self {
        Self::default()
    }
    pub fn from_source(value: bool, source: &str, confidence: f32) -> Self {
        EnabledState {
            value,
            source: source.to_string(),
            confidence,
        }
    }
}

/// Read-only-ness, with the same provenance discipline as [`EnabledState`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadOnlyState {
    pub value: bool,
    pub source: String,
    pub confidence: f32,
}

impl Default for ReadOnlyState {
    fn default() -> Self {
        ReadOnlyState {
            value: false,
            source: "default".to_string(),
            confidence: 0.5,
        }
    }
}

/// Node state: the mutable, interaction-relevant properties.
///
/// Only the fields that apply to a role are `Some`/true; serialization skips
/// the rest so the tree stays compact.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    /// Can receive keyboard focus.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focusable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,
    /// Item 28: provenance-tracked, never a bare bool.
    #[serde(default)]
    pub enabled: EnabledState,
    #[serde(default)]
    pub read_only: ReadOnlyState,
    /// Tree items: expanded (children visible) or collapsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    /// Scroll regions: is there content above / below / left / right?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_scroll: Option<ScrollEdges>,
    /// Tree depth (0 = root item).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// Text areas: the cursor column/row inside the node, if visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<(u16, u16)>,
}

/// Which edges of a scrollable region have more content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollEdges {
    #[serde(default)]
    pub up: bool,
    #[serde(default)]
    pub down: bool,
    #[serde(default)]
    pub left: bool,
    #[serde(default)]
    pub right: bool,
}

impl ScrollEdges {
    pub fn at_start(&self) -> bool {
        !self.up && !self.left
    }
    pub fn at_end(&self) -> bool {
        !self.down && !self.right
    }
}

/// One node of the semantic UI tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticNode {
    /// Stable ID (`screen`, `dialog/settings/button/save`, …) — same
    /// geometry-free scheme as controls/regions.
    pub id: String,
    pub role: Role,
    /// Parent node ID (`None` only for the root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Cell bounds.
    pub bounds: Bounds,
    /// Human-facing text (button label, field label, cell contents…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Current value (field contents, selected option, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub state: NodeState,
    /// Child nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SemanticNode>,
    /// Affordances this node itself advertises (key hints, shortcuts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affordances: Vec<Affordance>,
    /// Re-review P1 item 28: the joined identity — semantic id (this
    /// node's own id), native id, contract component name, and source
    /// loci — so a finding can move from rendered problem to source edit.
    /// `None` for pure inference with no joins (omitted on serialize).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::semantic::source_ref::ComponentIdentity>,
    pub confidence: Confidence,
}

impl SemanticNode {
    /// Find a node by ID (depth-first).
    pub fn find(&self, id: &str) -> Option<&SemanticNode> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(id))
    }

    /// Find a node by ID, mutably.
    pub fn find_mut(&mut self, id: &str) -> Option<&mut SemanticNode> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| c.find_mut(id))
    }

    /// All nodes matching a role, in tree order.
    pub fn collect_by_role<'a>(&'a self, role: &Role, out: &mut Vec<&'a SemanticNode>) {
        if &self.role == role {
            out.push(self);
        }
        for c in &self.children {
            c.collect_by_role(role, out);
        }
    }

    /// Indented text render (for logs / human diffing).
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_node(self, 0, &mut out);
        out
    }
}

fn render_node(node: &SemanticNode, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let label = node
        .label
        .as_deref()
        .map(|l| format!(" {l:?}"))
        .unwrap_or_default();
    let flags = state_flags(&node.state);
    out.push_str(&format!(
        "{}{} {} [{}x{} @ {},{}]{}{}\n",
        pad,
        node.role.slug(),
        node.id,
        node.bounds.width,
        node.bounds.height,
        node.bounds.x,
        node.bounds.y,
        label,
        flags
    ));
    for c in &node.children {
        render_node(c, depth + 1, out);
    }
}

fn state_flags(s: &NodeState) -> String {
    let mut v = Vec::new();
    if s.focusable {
        v.push("focusable");
    }
    if s.focused {
        v.push("focused");
    }
    if s.selected {
        v.push("selected");
    }
    if s.checked {
        v.push("checked");
    }
    if !s.enabled.value {
        v.push("disabled");
    }
    if s.read_only.value {
        v.push("read-only");
    }
    if let Some(e) = s.expanded {
        v.push(if e { "expanded" } else { "collapsed" });
    }
    if let Some(cs) = &s.can_scroll {
        let mut edges = Vec::new();
        if cs.up {
            edges.push("up");
        }
        if cs.down {
            edges.push("down");
        }
        if cs.left {
            edges.push("left");
        }
        if cs.right {
            edges.push("right");
        }
        let flags = format!("scroll:{}", edges.join("+"));
        // `v` holds &str flags; the scroll summary is owned, so render it
        // after the join instead of pushing into v.
        let head = if v.is_empty() {
            String::new()
        } else {
            format!(" ({})", v.join(", "))
        };
        return format!("{} ({})", head, flags);
    }
    if v.is_empty() {
        String::new()
    } else {
        format!(" ({})", v.join(", "))
    }
}

/// The built tree: root node plus layer tags for top-level subtrees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticTree {
    pub root: SemanticNode,
    /// Which layer each *top-level* child of the root occupies, by its ID.
    pub layers: HashMap<String, Layer>,
}

impl SemanticTree {
    /// Nodes of one layer (top-level subtree roots tagged with it).
    pub fn layer_nodes(&self, layer: Layer) -> Vec<&SemanticNode> {
        self.root
            .children
            .iter()
            .filter(|c| self.layers.get(&c.id) == Some(&layer))
            .collect()
    }

    pub fn render(&self) -> String {
        self.root.render()
    }

    /// Depth-first node list (root included).
    pub fn flatten(&self) -> Vec<&SemanticNode> {
        let mut out = Vec::new();
        fn walk<'a>(n: &'a SemanticNode, out: &mut Vec<&'a SemanticNode>) {
            out.push(n);
            for c in &n.children {
                walk(c, out);
            }
        }
        walk(&self.root, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: u16, y: u16, w: u16, h: u16) -> Bounds {
        Bounds {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn node(id: &str, role: Role, b: Bounds) -> SemanticNode {
        SemanticNode {
            id: id.to_string(),
            role,
            parent: None,
            bounds: b,
            label: None,
            value: None,
            state: NodeState::default(),
            children: Vec::new(),
            affordances: Vec::new(),
            identity: None,
            confidence: Confidence::inferred(0.9, &["test"]),
        }
    }

    #[test]
    fn find_walks_the_tree() {
        let mut root = node("screen", Role::Screen, bounds(0, 0, 80, 24));
        let mut table = node("table/hosts", Role::Table, bounds(0, 1, 60, 10));
        let row = node("table/hosts/row/1", Role::TableRow, bounds(0, 2, 60, 1));
        table.children.push(row);
        root.children.push(table);
        assert!(root.find("table/hosts/row/1").is_some());
        assert!(root.find("nope").is_none());
    }

    #[test]
    fn collect_by_role_finds_all() {
        let mut root = node("screen", Role::Screen, bounds(0, 0, 80, 24));
        root.children
            .push(node("row/a", Role::TableRow, bounds(0, 1, 10, 1)));
        root.children
            .push(node("row/b", Role::TableRow, bounds(0, 2, 10, 1)));
        root.children
            .push(node("btn/c", Role::Button, bounds(0, 3, 5, 1)));
        let mut rows = Vec::new();
        root.collect_by_role(&Role::TableRow, &mut rows);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn render_includes_flags() {
        let mut n = node("list/item/1", Role::ListItem, bounds(0, 5, 20, 1));
        n.state.selected = true;
        n.state.can_scroll = Some(ScrollEdges {
            up: false,
            down: true,
            left: false,
            right: false,
        });
        let text = n.render();
        assert!(text.contains("selected"), "{text}");
        assert!(text.contains("scroll:down"), "{text}");
    }

    #[test]
    fn enabled_state_defaults_are_honest() {
        let d = EnabledState::default();
        assert!(d.value, "default is enabled=true");
        assert_eq!(d.source, "default", "and the source says so");
        assert!(d.confidence < 1.0, "an assumption is not certain");
    }

    #[test]
    fn layer_filtering() {
        let mut tree = SemanticTree {
            root: node("screen", Role::Screen, bounds(0, 0, 80, 24)),
            layers: HashMap::new(),
        };
        tree.root
            .children
            .push(node("dialog/confirm", Role::Dialog, bounds(10, 5, 40, 8)));
        tree.root
            .children
            .push(node("footer/hints", Role::Footer, bounds(0, 23, 80, 1)));
        tree.layers.insert("dialog/confirm".into(), Layer::Modal);
        tree.layers.insert("footer/hints".into(), Layer::Status);
        assert_eq!(tree.layer_nodes(Layer::Modal).len(), 1);
        assert_eq!(tree.layer_nodes(Layer::Status).len(), 1);
        assert_eq!(tree.layer_nodes(Layer::Background).len(), 0);
    }

    #[test]
    fn scroll_edges_predicates() {
        let e = ScrollEdges {
            up: false,
            down: true,
            left: false,
            right: false,
        };
        assert!(e.at_start(), "no up content → at start");
        assert!(!e.at_end(), "down content → not at end");
    }

    /// Re-review item 36: the unified role→control projection. Every
    /// interactive role projects onto its control kind; structure roles
    /// project to None (they are not clickable things).
    #[test]
    fn control_kind_projection_is_total_and_total() {
        use crate::semantic::controls::ControlKind as K;
        assert_eq!(Role::Button.control_kind(), Some(K::Button));
        assert_eq!(Role::TextArea.control_kind(), Some(K::Field));
        assert_eq!(Role::Dropdown.control_kind(), Some(K::List));
        assert_eq!(Role::MenuItem.control_kind(), Some(K::MenuItem));
        // Structure is NOT a control.
        assert_eq!(Role::Dialog.control_kind(), None);
        assert_eq!(Role::Table.control_kind(), None);
        assert_eq!(Role::Unknown.control_kind(), None);
    }

    /// The native slug path and the unified projection agree: any slug
    /// role_from_slug accepts yields a control kind consistent with its
    /// own Role (no drift between the two shapes).
    #[test]
    fn native_slug_paths_never_drift() {
        use crate::semantic::native::{native_kind_for_test, native_role_for_test};
        for slug in [
            "button",
            "field",
            "textbox",
            "input",
            "checkbox",
            "radio",
            "tab",
            "list",
            "listitem",
            "menu",
            "menuitem",
            "table",
            "tree",
            "dialog",
            "panel",
            "progressbar",
            "spinner",
            "label",
            "hyperlink",
            "screen",
        ] {
            let role: Option<Role> = native_role_for_test(slug);
            let kind = native_kind_for_test(slug);
            assert_eq!(
                role.and_then(|r| r.control_kind()),
                kind,
                "slug '{slug}': role projection and kind mapping must agree"
            );
        }
    }
}
