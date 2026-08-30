// Integration tests for the state graph (spec item 15).

use tui_lab::exploration::{ExplorationBudget, StateGraph, StateId};
use tui_lab::session::SessionManager;

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

    assert!(graph.record_state("state_a", None, 0));
    assert!(graph.record_state("state_b", None, 1));
    assert!(!graph.record_state("state_a", None, 2));

    assert!(graph.state_count() >= 1);
    assert!(graph.has_state("state_a"));
    assert!(graph.has_state("state_b"));
    assert!(!graph.has_state("state_c"));

    let node_a = graph.get_node("state_a").unwrap();
    assert_eq!(node_a.visit_count, 2);
}

#[test]
fn test_state_graph_transitions() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    graph.record_transition("a", "b", "tab");
    graph.record_transition("b", "c", "tab");
    graph.record_transition("a", "b", "tab");

    assert_eq!(graph.transition_count(), 2);

    let out_a = graph.outgoing("a");
    assert_eq!(out_a.len(), 1);
    assert_eq!(out_a[0].count, 2);

    let in_c = graph.incoming("c");
    assert_eq!(in_c.len(), 1);
    assert_eq!(in_c[0].action_name, "tab");
}

#[test]
fn test_state_graph_novelty_scoring() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    // New state should have high novelty
    assert_eq!(graph.novelty_score("new"), 10);

    // Record once
    graph.record_state("once", None, 0);
    assert_eq!(graph.novelty_score("once"), 3);

    // Record many times
    for _ in 0..10 {
        graph.record_state("many", None, 0);
    }
    assert_eq!(graph.novelty_score("many"), -5);
}

#[test]
fn test_state_graph_dead_ends() {
    let budget = ExplorationBudget::default();
    let mut graph = StateGraph::new(budget);

    graph.record_transition("start", "middle", "tab");
    graph.record_transition("middle", "end", "enter");

    let dead_ends = graph.find_dead_ends();
    assert_eq!(dead_ends.len(), 1);
    assert_eq!(dead_ends[0].id.as_str(), "end");
}

#[test]
fn test_state_graph_budget_exhaustion() {
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
fn test_state_graph_merge() {
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
    assert_eq!(g1.get_node("b").unwrap().visit_count, 2);
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
    let novel = graph.record_state(&before.structure_hash, None, 0);
    assert!(novel);

    // Send a tab and record transition
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

    graph.record_transition(&before.structure_hash, &after.structure_hash, "tab");

    assert!(graph.state_count() >= 1);
    assert_eq!(graph.transition_count(), 1);
}
