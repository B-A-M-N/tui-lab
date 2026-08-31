//! The FocusGraph (Wave D items 36–37): focus transitions as a real graph,
//! keyed on stable control IDs.
//!
//! The old "focus graph" was a flat list of `(time, session, from-label,
//! to-label)` tuples. Labels are display metadata — they collide ("Save" in
//! two dialogs), they survive renames in history they no longer match, and a
//! list proves nothing about order. This module is the replacement:
//!
//! * Nodes are stable control IDs (the geometry-free IDs from
//!   [`crate::semantic::controls`]); a label snapshot travels as evidence
//!   only.
//! * Edges are `{from, to, via, count}` — `via` is the key that produced the
//!   transition, so Tab order A→B→C is *proven*, not suggested.
//! * [`FocusGraph::tab_cycle`] detects an actual cycle in the Tab subgraph
//!   (A→B→C→A), and [`FocusGraph::reverse_tab_consistent`] proves Shift+Tab
//!   is the true inverse (each forward edge has the matching reverse edge)
//!   rather than "some keys were sent and nothing crashed".

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One focus-transition edge (item 36). Keyed by stable control ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEdge {
    pub from: String,
    pub to: String,
    /// The input that produced the transition (`tab`, `shift+tab`,
    /// `mouse_click`, `enter`, …). Unknown provenance is `"unknown"`.
    pub via: String,
    pub count: u32,
}

/// A node: a control known to have held focus at least once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusNode {
    pub control_id: String,
    /// Label at first sighting (evidence only — never the key).
    pub label: Option<String>,
    pub visit_count: u32,
}

/// Where a transition's provenance came from.
///
/// The `via` field on [`FocusEdge`] names the input; the caller decides
/// observe-vs-driven when forming edges. Kept as documentation of the two
/// recording paths (MCP observations vs. audit drivers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionSource {
    /// Observed between consecutive screen observations.
    Observe,
    /// Driven by an audit/explorer sending the key itself.
    Driven,
}

/// The focus graph for one run (all sessions merged). Edges arrive
/// explicitly — callers (MCP observe, audit drivers, explorer) already track
/// the previous focus holder, so the graph keeps no hidden per-session
/// state and stays fully serializable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusGraph {
    pub nodes: HashMap<String, FocusNode>,
    /// Ordered edge insertion (deterministic serialization).
    pub edges: Vec<FocusEdge>,
}

impl FocusGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observed focus transition from → to with the input that
    /// produced it. Same-holder transitions and missing IDs are no-ops (an
    /// edge needs both ends keyed by stable ID; label-only observations
    /// cannot join the graph and the legacy label ledger carries them).
    pub fn transition(
        &mut self,
        from_id: &str,
        to_id: &str,
        label: Option<&str>,
        via: &str,
    ) -> Option<FocusEdge> {
        if from_id == to_id || from_id.is_empty() || to_id.is_empty() {
            return None;
        }
        Some(self.record_edge(from_id, to_id, via, label))
    }

    /// Directly record an edge (drivers that already validated both ends).
    pub fn record_edge(
        &mut self,
        from: &str,
        to: &str,
        via: &str,
        label: Option<&str>,
    ) -> FocusEdge {
        let node = self
            .nodes
            .entry(from.to_string())
            .or_insert_with(|| FocusNode {
                control_id: from.to_string(),
                label: None,
                visit_count: 0,
            });
        node.visit_count += 1;
        if node.label.is_none() {
            node.label = label.map(str::to_string);
        }
        let tnode = self
            .nodes
            .entry(to.to_string())
            .or_insert_with(|| FocusNode {
                control_id: to.to_string(),
                label: label.map(str::to_string),
                visit_count: 0,
            });
        tnode.visit_count += 1;

        if let Some(e) = self
            .edges
            .iter_mut()
            .find(|e| e.from == from && e.to == to && e.via == via)
        {
            e.count += 1;
            e.clone()
        } else {
            let e = FocusEdge {
                from: from.to_string(),
                to: to.to_string(),
                via: via.to_string(),
                count: 1,
            };
            self.edges.push(e.clone());
            e
        }
    }

    /// Outgoing edges whose provenance is `via` (e.g. every Tab successor of
    /// a control).
    pub fn successors(&self, from: &str, via: &str) -> Vec<&FocusEdge> {
        self.edges
            .iter()
            .filter(|e| e.from == from && e.via == via)
            .collect()
    }

    /// All edges with the given provenance, in insertion order.
    pub fn successors_all(&self, via: &str) -> Vec<&FocusEdge> {
        self.edges.iter().filter(|e| e.via == via).collect()
    }

    /// Detect a Tab cycle (item 37): walk forward Tab edges from any node
    /// until a repeat; a repeat proves A→B→…→A. Returns the cycle in order.
    pub fn tab_cycle(&self) -> Option<Vec<String>> {
        self.cycle_via("tab")
    }

    /// Cycle detection for any via-key (`shift+tab`, arrows…).
    pub fn cycle_via(&self, via: &str) -> Option<Vec<String>> {
        let starts: Vec<&str> = self
            .edges
            .iter()
            .filter(|e| e.via == via)
            .map(|e| e.from.as_str())
            .collect();
        for start in starts {
            if let Some(c) = self.walk_cycle(start, via) {
                return Some(c);
            }
        }
        None
    }

    fn walk_cycle(&self, start: &str, via: &str) -> Option<Vec<String>> {
        let mut path: Vec<String> = vec![start.to_string()];
        let mut cur = start.to_string();
        for _ in 0..self.nodes.len().max(1) {
            let next = self
                .edges
                .iter()
                .find(|e| e.from == cur && e.via == via)
                .map(|e| e.to.clone())?;
            if let Some(pos) = path.iter().position(|p| p == &next) {
                return Some(path.split_off(pos));
            }
            path.push(next.clone());
            cur = next;
        }
        None
    }

    /// Shift+Tab reverse-consistency (item 37): every Tab edge A→B must have
    /// a Shift+Tab edge B→A, and vice versa. Returns the missing inverse
    /// edges — an empty list proves true reversal, which the old label list
    /// could never do.
    pub fn reverse_tab_gaps(&self) -> Vec<(String, String)> {
        let has = |from: &str, to: &str, via: &str| {
            self.edges
                .iter()
                .any(|e| e.from == from && e.to == to && e.via == via)
        };
        let mut gaps = Vec::new();
        for e in &self.edges {
            match e.via.as_str() {
                "tab" => {
                    if !has(&e.to, &e.from, "shift+tab") {
                        gaps.push((e.to.clone(), e.from.clone()));
                    }
                }
                "shift+tab" => {
                    if !has(&e.to, &e.from, "tab") {
                        gaps.push((e.to.clone(), e.from.clone()));
                    }
                }
                _ => {}
            }
        }
        gaps
    }

    /// Serializable summary for MCP responses.
    pub fn summary(&self) -> serde_json::Value {
        let tab_edges: Vec<&FocusEdge> = self.edges.iter().filter(|e| e.via == "tab").collect();
        serde_json::json!({
            "nodes": self.nodes.len(),
            "edges": self.edges.len(),
            "tab_edges": tab_edges.len(),
            "tab_cycle": self.tab_cycle(),
            "reverse_tab_gaps": self.reverse_tab_gaps(),
        })
    }

    /// Merge another graph (multi-run aggregation). Edge counts add; node
    /// visit counts add; first-seen labels stick.
    pub fn merge(&mut self, other: &FocusGraph) {
        for (id, n) in &other.nodes {
            let e = self.nodes.entry(id.clone()).or_insert_with(|| FocusNode {
                control_id: id.clone(),
                label: None,
                visit_count: 0,
            });
            e.visit_count += n.visit_count;
            if e.label.is_none() {
                e.label = n.label.clone();
            }
        }
        for edge in &other.edges {
            if let Some(existing) = self
                .edges
                .iter_mut()
                .find(|e| e.from == edge.from && e.to == edge.to && e.via == edge.via)
            {
                existing.count += edge.count;
            } else {
                self.edges.push(edge.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 36: edges key on IDs; labels ride along as evidence only. Two
    /// controls that both say "Save" are two nodes.
    #[test]
    fn edges_key_on_ids_not_labels() {
        let mut g = FocusGraph::new();
        g.transition(
            "dialog/one/button/save",
            "dialog/two/button/save",
            Some("Save"),
            "tab",
        );
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].from, "dialog/one/button/save");
        assert_eq!(g.edges[0].to, "dialog/two/button/save");
        assert_eq!(g.nodes.len(), 2, "same-label controls are distinct nodes");
        // Same holder again: no edge.
        assert!(g
            .transition(
                "dialog/two/button/save",
                "dialog/two/button/save",
                None,
                "tab"
            )
            .is_none());
        assert_eq!(g.edges.len(), 1);
    }

    /// Item 36: `via` provenance distinguishes Tab from click edges between
    /// the same pair.
    #[test]
    fn via_provenance_kept_separate() {
        let mut g = FocusGraph::new();
        g.record_edge("a", "b", "tab", None);
        g.record_edge("a", "b", "mouse_click", None);
        g.record_edge("a", "b", "tab", None);
        assert_eq!(g.edges.len(), 2);
        assert_eq!(g.successors("a", "tab")[0].count, 2);
    }

    /// Item 37, the motivating proof: A→B→C→A via Tab is detected as a
    /// cycle.
    #[test]
    fn tab_cycle_detected() {
        let mut g = FocusGraph::new();
        for (f, t) in [("a", "b"), ("b", "c"), ("c", "a")] {
            g.record_edge(f, t, "tab", None);
        }
        let cyc = g.tab_cycle().expect("cycle");
        assert_eq!(cyc.len(), 3);
        assert!(
            cyc.contains(&"a".to_string())
                && cyc.contains(&"b".to_string())
                && cyc.contains(&"c".to_string())
        );
    }

    #[test]
    fn no_cycle_in_open_chain() {
        let mut g = FocusGraph::new();
        for (f, t) in [("a", "b"), ("b", "c")] {
            g.record_edge(f, t, "tab", None);
        }
        assert!(g.tab_cycle().is_none());
    }

    /// Item 37: true Shift+Tab reversal is proven edge-for-edge; a missing
    /// inverse is named.
    #[test]
    fn reverse_tab_consistency() {
        let mut g = FocusGraph::new();
        // Forward chain complete, reverse only covers b→a.
        g.record_edge("a", "b", "tab", None);
        g.record_edge("b", "c", "tab", None);
        g.record_edge("b", "a", "shift+tab", None);
        let gaps = g.reverse_tab_gaps();
        assert_eq!(
            gaps,
            vec![("c".to_string(), "b".to_string())],
            "the missing c→b inverse is named, not shrugged at"
        );

        // Completing the inverse proves consistency.
        g.record_edge("c", "b", "shift+tab", None);
        assert!(g.reverse_tab_gaps().is_empty());
    }

    #[test]
    fn merge_adds_counts() {
        let mut g1 = FocusGraph::new();
        g1.record_edge("a", "b", "tab", None);
        let mut g2 = FocusGraph::new();
        g2.record_edge("a", "b", "tab", None);
        g2.record_edge("b", "c", "tab", None);
        g1.merge(&g2);
        assert_eq!(g1.successors("a", "tab")[0].count, 2);
        assert_eq!(g1.edges.len(), 2);
        assert_eq!(g1.nodes.len(), 3);
    }

    /// Serialization round-trip: persisted graphs stay readable.
    #[test]
    fn json_roundtrip() {
        let mut g = FocusGraph::new();
        g.record_edge("a", "b", "tab", Some("Alpha"));
        let json = serde_json::to_string(&g).expect("serialize");
        let back: FocusGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.edges, g.edges);
        assert_eq!(back.nodes["a"].label.as_deref(), Some("Alpha"));
    }
}
