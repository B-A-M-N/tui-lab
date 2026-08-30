// Integration tests for the state graph (spec item 15).
//
// Keyed through the layered [`StateIdentity`] path (re-review P0 fix 3):
// the legacy structure-hash recorders are deprecated and no product or test
// path uses them.

use tui_lab::exploration::state_graph::StateIdentity;
use tui_lab::exploration::{ExplorationBudget, StateGraph, StateId};
use tui_lab::session::SessionManager;

/// Identity for a bare content key — tests exercise graph bookkeeping, not
/// frame semantics.
fn ident(key: &str) -> StateIdentity {
    StateIdentity::from_parts(key)
}

#[test]
fn test_exploration_budget_default() {
    let budget = ExplorationBudget::default();
    assert_eq!(budget.max_actions, 50);
    assert_eq!(budget.max_runtime_ms, 30_000);
    assert_eq!(budget.max_relaunches, 3);
    assert_eq!(budget.max_depth, 100);
    assert_eq!(budget.max_unique_states, 500);
}

#[test]
fn test_state_graph_record_and_query() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    let a = ident("state_a");
    let b = ident("state_b");
    assert!(graph.record_state_identity(&a, 0));
    assert!(graph.record_state_identity(&b, 1));
    assert!(!graph.record_state_identity(&a, 2));

    assert!(graph.state_count() >= 1);
    assert!(graph.has_state(a.id().as_str()));
    assert!(graph.has_state(b.id().as_str()));
    assert!(!graph.has_state(ident("state_c").id().as_str()));

    let node_a = graph.get_node(a.id().as_str()).unwrap();
    assert_eq!(node_a.visit_count, 2);
}

#[test]
fn test_state_graph_transitions() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    let a = ident("a");
    let b = ident("b");
    let c = ident("c");

    graph.record_transition_identity(&a, &b, "tab");
    graph.record_transition_identity(&b, &c, "tab");
    graph.record_transition_identity(&a, &b, "tab");

    assert_eq!(graph.transition_count(), 2);

    let out_a = graph.outgoing(a.id().as_str());
    assert_eq!(out_a.len(), 1);
    assert_eq!(out_a[0].count, 2);

    let in_c = graph.incoming(c.id().as_str());
    assert_eq!(in_c.len(), 1);
    assert_eq!(in_c[0].action_name, "tab");
}

#[test]
fn test_state_graph_novelty_scoring() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    // New state should have high novelty.
    assert_eq!(graph.novelty_score("new"), 10);

    // Record once (score keys on the node id — the identity digest).
    let once = ident("once");
    graph.record_state_identity(&once, 0);
    assert_eq!(graph.novelty_score(once.id().as_str()), 3);

    // Record many times.
    let many = ident("many");
    for _ in 0..10 {
        graph.record_state_identity(&many, 0);
    }
    assert_eq!(graph.novelty_score(many.id().as_str()), -5);
}

#[test]
fn test_state_graph_dead_ends() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    let start = ident("start");
    let middle = ident("middle");
    let end = ident("end");

    graph.record_transition_identity(&start, &middle, "tab");
    graph.record_transition_identity(&middle, &end, "enter");

    let dead_ends = graph.find_dead_ends();
    assert_eq!(dead_ends.len(), 1);
}

#[test]
fn test_state_graph_budget_exhaustion() {
    let budget = ExplorationBudget {
        max_unique_states: 2,
        ..Default::default()
    };
    let mut graph = StateGraph::new(budget);

    graph.record_state_identity(&ident("a"), 0);
    assert!(!graph.budget_exhausted());

    graph.record_state_identity(&ident("b"), 1);
    assert!(graph.budget_exhausted());
}

#[test]
fn test_state_graph_merge() {
    let budget = ExplorationBudget::default();
    let mut g1 = StateGraph::new(budget.clone());
    let mut g2 = StateGraph::new(budget);

    let a = ident("a");
    let b = ident("b");
    let c = ident("c");

    g1.record_state_identity(&a, 0);
    g1.record_state_identity(&b, 1);
    g1.record_transition_identity(&a, &b, "tab");

    g2.record_state_identity(&b, 10);
    g2.record_state_identity(&c, 11);
    g2.record_transition_identity(&b, &c, "enter");

    g1.merge(&g2);

    assert_eq!(g1.state_count(), 3);
    assert_eq!(g1.transition_count(), 2);
    assert_eq!(g1.get_node(b.id().as_str()).unwrap().visit_count, 2);
}

#[test]
fn test_state_id_creation() {
    let id1 = StateId::from_structure_hash("abc");
    let id2 = StateId::from_structure_hash("abc");
    let id3 = StateId::from_semantic_hash("def");

    assert_eq!(id1, id2);
    assert_ne!(id1, id3);
    assert_eq!(id1.as_str(), "abc");
    assert!(id3.as_str().starts_with("sem:"));
}

#[test]
fn test_exploration_run_with_graph() {
    let mut mgr = SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "import sys; print('test'); sys.stdout.flush(); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let budget = ExplorationBudget {
        max_actions: 5,
        max_unique_states: 10,
        ..Default::default()
    };
    let mut graph = StateGraph::new(budget);

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let before = sess.observe(30).unwrap();
    let before_identity = StateIdentity::from_frame(&before);
    assert!(graph.record_state_identity(&before_identity, 0));

    // Send a tab and record the transition through the identity path.
    let _ = sess.send(tui_lab::backend::Input::Key(
        tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Tab),
    ));
    let _ = sess.wait(
        tui_lab::backend::WaitCond::ScreenStable {
            quiet_for: std::time::Duration::from_millis(80),
            after_screen_seq: None,
        },
        500,
    );
    let after = sess.observe(50).unwrap();

    graph.record_transition_identity(
        &before_identity,
        &StateIdentity::from_frame(&after),
        "tab",
    );

    assert!(graph.state_count() >= 1);
    assert_eq!(graph.transition_count(), 1);
}
