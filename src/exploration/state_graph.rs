//! State graph for bounded exploration (spec item 15).
//!
//! Tracks visited UI states and the transitions between them during exploration.
//! Powers novelty scoring, dead-end detection, and guided candidate generation.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Identity for a UI state node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateId(String);

impl StateId {
    /// Create a state ID from a structure hash.
    pub fn from_structure_hash(hash: &str) -> Self {
        StateId(hash.to_string())
    }

    /// Create a state ID from a semantic hash.
    pub fn from_semantic_hash(hash: &str) -> Self {
        StateId(format!("sem:{}", hash))
    }

    /// Create a combined ID from multiple hashes.
    pub fn combined(parts: &[&str]) -> Self {
        let mut hasher = DefaultHasher::new();
        for p in parts {
            p.hash(&mut hasher);
        }
        StateId(format!("{:016x}", hasher.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A node in the state graph: a unique UI state.
#[derive(Debug, Clone)]
pub struct StateNode {
    pub id: StateId,
    pub first_seen_at: u64,
    pub visit_count: u32,
    pub structure_hash: String,
    pub semantic_hash: Option<String>,
}

/// A directed edge: transition between two states via an action.
#[derive(Debug, Clone)]
pub struct StateTransition {
    pub from: StateId,
    pub to: StateId,
    pub action_name: String,
    pub count: u32,
}

/// Bounded exploration run model (spec item 41).
#[derive(Debug, Clone)]
pub struct ExplorationBudget {
    pub max_actions: u32,
    pub max_runtime_ms: u64,
    pub max_relaunches: u32,
    pub max_depth: u32,
    pub max_unique_states: u32,
}

impl Default for ExplorationBudget {
    fn default() -> Self {
        ExplorationBudget {
            max_actions: 50,
            max_runtime_ms: 30_000,
            max_relaunches: 3,
            max_depth: 100,
            max_unique_states: 500,
        }
    }
}

/// The state graph: nodes (states) and edges (transitions).
#[derive(Debug)]
pub struct StateGraph {
    nodes: HashMap<StateId, StateNode>,
    edges: Vec<StateTransition>,
    budget: ExplorationBudget,
}

impl StateGraph {
    /// Create a new state graph with the given budget.
    pub fn new(budget: ExplorationBudget) -> Self {
        StateGraph {
            nodes: HashMap::new(),
            edges: Vec::new(),
            budget,
        }
    }

    /// Record observing a state. Returns true if this state is novel.
    pub fn record_state(
        &mut self,
        structure_hash: &str,
        semantic_hash: Option<&str>,
        timestamp: u64,
    ) -> bool {
        let id = StateId::from_structure_hash(structure_hash);

        let novel = !self.nodes.contains_key(&id);

        let node = self.nodes.entry(id.clone()).or_insert_with(|| StateNode {
            id,
            first_seen_at: timestamp,
            visit_count: 0,
            structure_hash: structure_hash.to_string(),
            semantic_hash: semantic_hash.map(String::from),
        });

        node.visit_count += 1;
        novel
    }

    /// Record a transition between two states.
    pub fn record_transition(&mut self, from_hash: &str, to_hash: &str, action_name: &str) {
        let from = StateId::from_structure_hash(from_hash);
        let to = StateId::from_structure_hash(to_hash);

        // Ensure both nodes exist
        self.nodes.entry(from.clone()).or_insert_with(|| StateNode {
            id: from.clone(),
            first_seen_at: 0,
            visit_count: 0,
            structure_hash: from_hash.to_string(),
            semantic_hash: None,
        });
        self.nodes.entry(to.clone()).or_insert_with(|| StateNode {
            id: to.clone(),
            first_seen_at: 0,
            visit_count: 0,
            structure_hash: to_hash.to_string(),
            semantic_hash: None,
        });

        // Find existing edge or create new
        if let Some(edge) = self
            .edges
            .iter_mut()
            .find(|e| e.from == from && e.to == to && e.action_name == action_name)
        {
            edge.count += 1;
        } else {
            self.edges.push(StateTransition {
                from,
                to,
                action_name: action_name.to_string(),
                count: 1,
            });
        }
    }

    /// Check if a state has been visited.
    pub fn has_state(&self, structure_hash: &str) -> bool {
        self.nodes
            .contains_key(&StateId::from_structure_hash(structure_hash))
    }

    /// Get a node by hash.
    pub fn get_node(&self, structure_hash: &str) -> Option<&StateNode> {
        self.nodes
            .get(&StateId::from_structure_hash(structure_hash))
    }

    /// Get all outgoing edges from a state.
    pub fn outgoing(&self, structure_hash: &str) -> Vec<&StateTransition> {
        let id = StateId::from_structure_hash(structure_hash);
        self.edges.iter().filter(|e| e.from == id).collect()
    }

    /// Get all incoming edges to a state.
    pub fn incoming(&self, structure_hash: &str) -> Vec<&StateTransition> {
        let id = StateId::from_structure_hash(structure_hash);
        self.edges.iter().filter(|e| e.to == id).collect()
    }

    /// Count of unique states.
    pub fn state_count(&self) -> usize {
        self.nodes.len()
    }

    /// Count of unique transitions.
    pub fn transition_count(&self) -> usize {
        self.edges.len()
    }

    /// Check if budget is exhausted.
    pub fn budget_exhausted(&self) -> bool {
        self.nodes.len() as u32 >= self.budget.max_unique_states
    }

    /// Get the budget.
    pub fn budget(&self) -> &ExplorationBudget {
        &self.budget
    }

    /// Find dead-end states (states with no outgoing transitions, excluding exit states).
    pub fn find_dead_ends(&self) -> Vec<&StateNode> {
        self.nodes
            .values()
            .filter(|node| {
                let out = self.outgoing(&node.id.0);
                // Dead-end if no outgoing edges but has incoming
                out.is_empty() && !self.incoming(&node.id.0).is_empty()
            })
            .collect()
    }

    /// Calculate novelty score for a candidate state.
    /// Higher = more novel/interesting.
    pub fn novelty_score(&self, structure_hash: &str) -> i32 {
        let mut score = 0i32;

        // Never visited: very high novelty
        if !self.has_state(structure_hash) {
            score += 10;
        } else if let Some(node) = self.get_node(structure_hash) {
            // Visited few times: some novelty
            if node.visit_count <= 2 {
                score += 3;
            }
            // Visited many times: penalize
            if node.visit_count > 5 {
                score -= 5;
            }
        }

        score
    }

    /// Get all known state hashes.
    pub fn known_states(&self) -> Vec<&str> {
        self.nodes.keys().map(|k| k.as_str()).collect()
    }

    /// Get edges as (from_hash, to_hash, action) tuples.
    pub fn edge_list(&self) -> Vec<(&str, &str, &str)> {
        self.edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.action_name.as_str()))
            .collect()
    }

    /// Get node visit counts.
    pub fn visit_counts(&self) -> Vec<(&str, u32)> {
        self.nodes
            .values()
            .map(|n| (n.id.as_str(), n.visit_count))
            .collect()
    }

    /// Serializable snapshot for run persistence (`state_graph.json`).
    pub fn export(&self) -> serde_json::Value {
        serde_json::json!({
            "states": self.nodes.values().map(|n| serde_json::json!({
                "structure_hash": n.structure_hash,
                "visit_count": n.visit_count,
                "first_seen_at": n.first_seen_at,
                            })).collect::<Vec<_>>(),
            "transitions": self.edges.iter().map(|e| serde_json::json!({
                "from": e.from.as_str(),
                "to": e.to.as_str(),
                "action": e.action_name,
                "count": e.count,
            })).collect::<Vec<_>>(),
        })
    }

    /// Merge another graph into this one.
    pub fn merge(&mut self, other: &StateGraph) {
        for (id, node) in &other.nodes {
            let entry = self.nodes.entry(id.clone()).or_insert_with(|| node.clone());
            entry.visit_count += node.visit_count;
        }
        for edge in &other.edges {
            if let Some(existing) = self.edges.iter_mut().find(|e| {
                e.from == edge.from && e.to == edge.to && e.action_name == edge.action_name
            }) {
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

    #[test]
    fn test_state_id() {
        let id1 = StateId::from_structure_hash("abc123");
        let id2 = StateId::from_structure_hash("abc123");
        let id3 = StateId::from_structure_hash("xyz789");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_record_state_novel() {
        let budget = ExplorationBudget::default();
        let mut graph = StateGraph::new(budget);

        assert!(graph.record_state("hash1", None, 0));
        assert!(!graph.record_state("hash1", None, 1));
        assert!(graph.record_state("hash2", None, 2));

        assert_eq!(graph.state_count(), 2);
    }

    #[test]
    fn test_record_transition() {
        let budget = ExplorationBudget::default();
        let mut graph = StateGraph::new(budget);

        graph.record_state("a", None, 0);
        graph.record_state("b", None, 1);
        graph.record_transition("a", "b", "tab");
        graph.record_transition("a", "b", "tab"); // duplicate

        assert_eq!(graph.transition_count(), 1);
        let out = graph.outgoing("a");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].count, 2);
    }

    #[test]
    fn test_novelty_score() {
        let budget = ExplorationBudget::default();
        let mut graph = StateGraph::new(budget);

        // New state
        assert_eq!(graph.novelty_score("new_hash"), 10);

        // Record and check
        graph.record_state("visited_once", None, 0);
        assert_eq!(graph.novelty_score("visited_once"), 3);

        // Visit many times
        for _ in 0..10 {
            graph.record_state("visited_many", None, 0);
        }
        assert_eq!(graph.novelty_score("visited_many"), -5);
    }

    #[test]
    fn test_budget_exhausted() {
        let budget = ExplorationBudget {
            max_unique_states: 2,
            ..Default::default()
        };
        let mut graph = StateGraph::new(budget);

        graph.record_state("a", None, 0);
        assert!(!graph.budget_exhausted());

        graph.record_state("b", None, 1);
        assert!(graph.budget_exhausted());
    }

    #[test]
    fn test_dead_ends() {
        let budget = ExplorationBudget::default();
        let mut graph = StateGraph::new(budget);

        graph.record_state("start", None, 0);
        graph.record_state("middle", None, 1);
        graph.record_state("dead_end", None, 2);

        graph.record_transition("start", "middle", "tab");
        graph.record_transition("middle", "dead_end", "enter");

        let dead_ends = graph.find_dead_ends();
        assert_eq!(dead_ends.len(), 1);
        assert_eq!(dead_ends[0].id.as_str(), "dead_end");
    }

    #[test]
    fn test_merge() {
        let budget = ExplorationBudget::default();
        let mut g1 = StateGraph::new(budget.clone());
        let mut g2 = StateGraph::new(budget);

        g1.record_state("a", None, 0);
        g1.record_state("b", None, 1);
        g1.record_transition("a", "b", "tab");

        g2.record_state("b", None, 10);
        g2.record_state("c", None, 11);
        g2.record_transition("b", "c", "enter");

        g1.merge(&g2);

        assert_eq!(g1.state_count(), 3);
        assert_eq!(g1.transition_count(), 2);

        // State b should have combined visit count
        let b = g1.get_node("b").unwrap();
        assert_eq!(b.visit_count, 2);
    }
}
