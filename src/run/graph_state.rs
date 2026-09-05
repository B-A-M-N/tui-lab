//! Cohesive state for the run's semantic/exploration graphs.
//!
//! Round-2 decomposition (G1): the focus-transition ledger, the ID-keyed
//! focus graph, and the exploration state graph move OUT of the flat run
//! bucket into this holder. All three are graph-shaped evidence about how
//! the UI was traversed; they are recorded from the same observations and
//! queried together by audits/exploration.
//!
//! `RunContext` keeps the public methods as thin delegations and exposes
//! the holder through `graphs()`/`graphs_mut()` for the callers that fold
//! over the graphs directly. No new locks — the graphs stay behind the
//! run's single mutex.

use crate::exploration::state_graph::{ExplorationBudget, StateGraph};

/// Focus history + exploration graph for one run.
pub struct RunGraphs {
    /// Focus-transition ledger: (unix_ms, session, from, to) recorded from
    /// semantic analysis of every observation. The run's focus graph
    /// (legacy label form, kept for display continuity).
    pub focus_transitions: Vec<(u64, String, Option<String>, Option<String>)>,
    /// The real FocusGraph (Wave D items 36–37): focus transitions as
    /// edges keyed on stable control IDs with the input that produced
    /// them, so Tab order and Shift+Tab reversal are provable, not
    /// suggested. Recorded from the same observations as
    /// `focus_transitions` (labels) plus the audit drivers (driven edges).
    pub focus_graph: crate::semantic::focus_graph::FocusGraph,
    /// Exploration state graph (owned here so audits/exploration share it).
    pub state_graph: StateGraph,
}

impl RunGraphs {
    /// A fresh, empty graph set.
    pub(super) fn new() -> Self {
        RunGraphs {
            focus_transitions: Vec::new(),
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
            state_graph: StateGraph::new(ExplorationBudget::default()),
        }
    }

    /// Record one focus transition into BOTH focus ledgers (Wave D item
    /// 36): the legacy label list for display continuity, and the
    /// control-ID graph for proof. `via` names the input that produced
    /// the transition; the graph only joins transitions whose ends carry
    /// stable control IDs — label-only observations stay in the legacy
    /// ledger.
    pub(super) fn record_observation(
        &mut self,
        session_id: &str,
        from_label: Option<String>,
        to_label: Option<String>,
        from_id: Option<&str>,
        to_id: Option<&str>,
        via: &str,
    ) {
        // Legacy label ledger (unchanged shape, skips no-op transitions).
        if from_label != to_label {
            self.focus_transitions.push((
                super::now_ms(),
                session_id.to_string(),
                from_label,
                to_label.clone(),
            ));
        }
        // ID-keyed graph.
        if let (Some(f), Some(t)) = (from_id, to_id) {
            self.focus_graph.transition(f, t, to_label.as_deref(), via);
        }
    }

    /// Record one legacy-only transition (no control IDs observed).
    pub(super) fn record_transition(
        &mut self,
        session_id: &str,
        from: Option<String>,
        to: Option<String>,
    ) {
        if from == to {
            return;
        }
        self.focus_transitions
            .push((super::now_ms(), session_id.to_string(), from, to));
    }
}
