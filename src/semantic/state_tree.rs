//! The hierarchical terminal-state tree (re-review Wave-4 /
//! "TerminalStateTree"): one machine-readable rendering of the implemented
//! UI — regions nested per containment, controls inside their regions,
//! focus marked, screen-level components attached.
//!
//! Flat lists (`SemanticScreen.regions`, `.controls`) answer "what exists";
//! the tree answers "what exists *where*". That is the shape an agent can
//! compare against an intended design (or hand to a builder) without
//! re-deriving containment from bounds.

use crate::semantic::controls::Control;
use crate::semantic::focus::FocusInfo;
use crate::semantic::regions::{Bounds, Region};
use crate::semantic::Component;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One node of the state tree: a region (possibly the synthetic root) with
/// the controls directly inside it and its child regions nested below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateNode {
    /// Region stable ID; `"screen"` for the synthetic root.
    pub id: String,
    /// Region kind slug, or `"screen"` for the root.
    pub kind: String,
    /// Region title, when detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Cell bounds.
    pub bounds: Bounds,
    /// Controls whose anchor cell lies inside this region (direct children —
    /// controls inside a nested region belong to the nested node).
    pub controls: Vec<ControlEntry>,
    /// Screen-level components overlapping this region.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
    /// Nested regions (containment children).
    pub children: Vec<StateNode>,
}

/// A control inside a region: the stable ID, role, label, and state flags
/// an auditor or builder needs at a glance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlEntry {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub focusable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    pub enabled: bool,
    /// Toggle state for checkboxes/radios; `None` for other kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// `Some(true)` only for controls whose `selected` flag is set
    /// (selected tab, chosen radio, highlighted list row).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
}

/// The full tree: a synthetic root spanning the viewport, holding
/// top-level regions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalStateTree {
    pub root: StateNode,
    /// Focus report from the semantic analysis (duplicated here so the tree
    /// is self-contained for consumers).
    pub focus: FocusInfo,
}

/// Build the tree from a semantic screen's parts.
pub fn build_state_tree(
    cols: u16,
    rows: u16,
    regions: &[Region],
    controls: &[Control],
    focus: &FocusInfo,
    components: &[Component],
) -> TerminalStateTree {
    let mut root = StateNode {
        id: "screen".to_string(),
        kind: "screen".to_string(),
        title: None,
        bounds: Bounds {
            x: 0,
            y: 0,
            width: cols,
            height: rows,
        },
        controls: Vec::new(),
        components: Vec::new(),
        children: Vec::new(),
    };

    // Region nodes by id, and each region's parent id (owned copies so the
    // nesting pass never borrows `regions` while mutating nodes).
    let mut nodes: HashMap<String, StateNode> = regions
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                StateNode {
                    id: r.id.clone(),
                    kind: format!("{:?}", r.kind).to_lowercase(),
                    title: r.title.clone(),
                    bounds: r.bounds.clone(),
                    controls: Vec::new(),
                    components: Vec::new(),
                    children: Vec::new(),
                },
            )
        })
        .collect();
    let parents: HashMap<String, Option<String>> = regions
        .iter()
        .map(|r| (r.id.clone(), r.parent_id.clone()))
        .collect();

    // Attach controls to their smallest containing region (the binding
    // `detect_controls` already computed), else the root.
    for c in controls {
        let entry = ControlEntry {
            id: c.id.clone(),
            kind: format!("{:?}", c.kind).to_lowercase(),
            label: c.label.clone(),
            value: c.value.clone(),
            focusable: c.focusable,
            focused: c.focused,
            enabled: c.enabled,
            checked: matches!(
                c.kind,
                crate::semantic::controls::ControlKind::Checkbox
                    | crate::semantic::controls::ControlKind::Radio
            )
            .then_some(c.checked),
            selected: c.selected.then_some(true),
        };
        match c.region_id.as_deref().and_then(|rid| nodes.get_mut(rid)) {
            Some(node) => node.controls.push(entry),
            None => root.controls.push(entry),
        }
    }

    // Attach components to the smallest region containing their midpoint;
    // otherwise the root.
    for comp in components {
        let b = comp.bounds();
        let (mid_x, mid_y) = (b.x + b.width / 2, b.y + b.height / 2);
        let target = nodes
            .values_mut()
            .filter(|n| {
                let nb = &n.bounds;
                mid_x >= nb.x
                    && mid_y >= nb.y
                    && mid_x < nb.x + nb.width
                    && mid_y < nb.y + nb.height
            })
            .min_by_key(|n| n.bounds.width as u32 * n.bounds.height as u32);
        match target {
            Some(node) => node.components.push(comp.clone()),
            None => root.components.push(comp.clone()),
        }
    }

    // Nest regions under their parents (and top-level ones under the root),
    // deepest-last: regions arrive in arbitrary order, so take any node
    // whose parent is already materialized and repeat until done.
    // Where each materialized node now lives, so a child can find its
    // parent after the parent left `nodes`: "root" or the parent's id.
    let mut placed_under: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<String> = regions.iter().map(|r| r.id.clone()).collect();
    let mut orphans: HashMap<String, StateNode> = HashMap::new();
    loop {
        let mut materialized: Vec<String> = Vec::new();
        let mut still_pending: Vec<String> = Vec::new();
        for id in &pending {
            // Parent placement: materialized in this or an earlier pass
            // (tracked in placed_under), still queued (in nodes), or the
            // synthetic root.
            let parent = parents.get(id).cloned().flatten().filter(|pid| {
                pid == "screen" || nodes.contains_key(pid) || placed_under.contains_key(pid)
            });
            match parent {
                Some(pid) => {
                    let node = nodes.remove(id).expect("pending id exists");
                    if pid == "screen" {
                        root.children.push(node);
                        placed_under.insert(id.clone(), "screen".into());
                    } else if let Some(pnode) = nodes.get_mut(&pid) {
                        pnode.children.push(node);
                        placed_under.insert(id.clone(), pid.clone());
                    } else if let Some(pnode) = orphans.get_mut(&pid) {
                        pnode.children.push(node);
                        placed_under.insert(id.clone(), pid.clone());
                    } else {
                        // Parent materialized earlier: route through the
                        // tree from the root down.
                        if attach_under(&mut root, &pid, node.clone()) {
                            placed_under.insert(id.clone(), pid.clone());
                        } else {
                            orphans.insert(id.clone(), node);
                            continue;
                        }
                    }
                }
                None => {
                    if let Some(node) = nodes.remove(id) {
                        root.children.push(node);
                        placed_under.insert(id.clone(), "screen".into());
                    }
                }
            }
            materialized.push(id.clone());
        }
        // Orphans whose parent just materialized re-enter the queue.
        let reclaimed: Vec<String> = orphans
            .keys()
            .filter(|oid| {
                parents
                    .get(*oid)
                    .cloned()
                    .flatten()
                    .is_some_and(|pid| materialized.contains(&pid))
            })
            .cloned()
            .collect();
        for oid in reclaimed {
            if let Some(node) = orphans.remove(&oid) {
                nodes.insert(oid, node);
            }
        }
        still_pending.retain(|id| !materialized.contains(id));
        still_pending.extend(orphans.keys().cloned());
        if materialized.is_empty() || still_pending.is_empty() {
            // Nothing progressed (cyclic parent links cannot happen —
            // assign_hierarchy is acyclic) or everything placed.
            for (_, node) in orphans.drain() {
                root.children.push(node);
            }
            break;
        }
        pending = still_pending;
    }

    TerminalStateTree {
        root,
        focus: focus.clone(),
    }
}

impl TerminalStateTree {
    /// Indented text rendering (for logs and human diffs).
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_node(&self.root, 0, &mut out);
        out
    }
}

/// Attach `node` under the descendant named `pid`, searching from `root`.
/// True when the parent was found.
fn attach_under(root: &mut StateNode, pid: &str, node: StateNode) -> bool {
    fn walk(node: &mut StateNode, pid: &str, child: StateNode) -> bool {
        if node.id == pid {
            node.children.push(child);
            return true;
        }
        for c in node.children.iter_mut() {
            if walk(c, pid, child.clone()) {
                return true;
            }
        }
        false
    }
    walk(root, pid, node)
}

fn render_node(node: &StateNode, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let title = node
        .title
        .as_deref()
        .map(|t| format!(" \"{}\"", t))
        .unwrap_or_default();
    out.push_str(&format!(
        "{}{} {} [{}x{} @ {},{}]{}\n",
        pad,
        node.kind,
        node.id,
        node.bounds.width,
        node.bounds.height,
        node.bounds.x,
        node.bounds.y,
        title
    ));
    for c in &node.controls {
        out.push_str(&format!("{}  {} {} ({})\n", pad, c.kind, c.label, c.id));
    }
    for child in &node.children {
        render_node(child, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::confidence::Confidence;
    use crate::semantic::controls::{Control, ControlBounds, ControlKind};
    use crate::semantic::regions::{ClippingState, RegionKind};

    fn region(id: &str, parent: Option<&str>, x: u16, y: u16, w: u16, h: u16) -> Region {
        Region {
            id: id.to_string(),
            kind: RegionKind::Panel,
            title: None,
            bounds: Bounds {
                x,
                y,
                width: w,
                height: h,
            },
            confidence: Confidence::inferred(0.9, &["test"]),
            parent_id: parent.map(String::from),
            child_ids: Vec::new(),
            clipping_state: ClippingState::None,
        }
    }

    fn control(id: &str, label: &str, region: Option<&str>) -> Control {
        Control {
            id: id.to_string(),
            kind: ControlKind::Button,
            label: label.to_string(),
            value: None,
            bounds: ControlBounds {
                x: 1,
                y: 1,
                width: 5,
                height: 1,
            },
            region_id: region.map(String::from),
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: Confidence::inferred(0.9, &["test"]),
            evidence: Vec::new(),
            source: "inferred".to_string(),
        }
    }

    fn focus() -> FocusInfo {
        FocusInfo {
            control: None,
            control_id: None,
            confidence: 0.1,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn nested_regions_nest_in_the_tree() {
        let regions = vec![
            region("outer", None, 0, 0, 40, 20),
            region("inner", Some("outer"), 2, 2, 20, 8),
        ];
        let controls = vec![
            control("button/ok", "OK", Some("inner")),
            control("button/cancel", "Cancel", Some("outer")),
            control("button/loose", "Loose", None),
        ];
        let tree = build_state_tree(80, 24, &regions, &controls, &focus(), &[]);

        // Root holds `outer`; `outer` holds `inner` plus the Cancel button;
        // `inner` holds OK; the root keeps the region-less control.
        assert_eq!(tree.root.children.len(), 1, "outer nests under root");
        let outer = &tree.root.children[0];
        assert_eq!(outer.id, "outer");
        assert_eq!(outer.children.len(), 1);
        assert_eq!(outer.children[0].id, "inner");
        assert_eq!(
            outer.children[0]
                .controls
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["button/ok"],
            "inner region owns its control"
        );
        assert!(
            outer.controls.iter().any(|c| c.id == "button/cancel"),
            "outer owns its direct control"
        );
        assert!(
            tree.root.controls.iter().any(|c| c.id == "button/loose"),
            "region-less control lands on the root"
        );
    }

    /// Components attach to the smallest containing region.
    #[test]
    fn components_attach_to_smallest_region() {
        use crate::semantic::components::{Component, ScrollbarComponent, ScrollbarOrientation};
        let regions = vec![region("panel", None, 0, 0, 40, 20)];
        let sb = Component::Scrollbar(ScrollbarComponent {
            orientation: ScrollbarOrientation::Vertical,
            track_x: 39,
            track_y: 0,
            track_len: 20,
            thumb_offset: 0,
            thumb_len: 5,
            confidence: Confidence::inferred(0.8, &["bar-glyphs"]),
        });
        let tree = build_state_tree(80, 24, &regions, &[], &focus(), std::slice::from_ref(&sb));
        assert!(
            tree.root.children[0]
                .components
                .iter()
                .any(|c| matches!(c, Component::Scrollbar(_))),
            "scrollbar attached to the panel"
        );
    }

    /// Toggle state flows into the entries; non-toggles omit it.
    #[test]
    fn control_entries_carry_state_flags() {
        let regions: Vec<Region> = Vec::new();
        let mut cb = control("checkbox/save", "Auto-save", None);
        cb.kind = ControlKind::Checkbox;
        cb.checked = true;
        let mut tab = control("tab/general", "General", None);
        tab.kind = ControlKind::Tab;
        tab.selected = true;
        let tree = build_state_tree(80, 24, &regions, &[cb, tab], &focus(), &[]);
        let entries = &tree.root.controls;
        let cb_entry = entries.iter().find(|c| c.id == "checkbox/save").unwrap();
        assert_eq!(cb_entry.checked, Some(true));
        let tab_entry = entries.iter().find(|c| c.id == "tab/general").unwrap();
        assert_eq!(tab_entry.selected, Some(true));
        assert_eq!(tab_entry.checked, None, "tabs do not carry checked");
    }

    /// The text render is a stable, human-readable form.
    #[test]
    fn render_is_indented() {
        let regions = vec![region("panel", None, 0, 0, 40, 20)];
        let tree = build_state_tree(
            80,
            24,
            &regions,
            &[control("button/ok", "OK", Some("panel"))],
            &focus(),
            &[],
        );
        let text = tree.render();
        assert!(text.contains("screen screen"));
        assert!(text.contains("  panel panel"));
        assert!(text.contains("    button OK (button/ok)"));
    }
}
