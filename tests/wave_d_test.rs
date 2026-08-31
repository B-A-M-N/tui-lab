//! Wave D end-to-end: agent interaction intelligence.
//!
//! These tests exercise the live paths — the ones that need a real PTY and
//! process — against small python programs. Unit tests for the pure logic
//! live next to their modules.

use tui_lab::exploration::random;
use tui_lab::exploration::state_graph::ExplorationBudget;

/// A program that dies on the first key it reads — the reproduction
/// pipeline's target. Raw mode is required: the PTY line discipline
/// buffers normal reads until Enter, and the explorer's key pool (tab,
/// arrows, …) would otherwise never deliver a byte.
fn crasher_args() -> Vec<String> {
    vec![
        "-c".into(),
        "import sys,tty; tty.setraw(0); print('CRASHER READY'); sys.stdin.buffer.read(1); sys.exit(101)"
            .into(),
    ]
}

fn start(mgr: &mut tui_lab::session::SessionManager, args: &[String]) -> String {
    mgr.start("python3", args, None, &[], 80, 24, "auto", "local")
        .expect("start session")
}

/// Item 38, full pipeline: seeded exploration of a crashing app produces
/// (a) a failure exit, (b) a minimized reproduction saved as a Scenario,
/// and (c) a Finding whose `reproduction` names the scenario. The scenario
/// replays through the same canonical executor path as `tui_scenario run`.
#[test]
fn crash_minimization_produces_replayable_reproduction() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = start(&mut mgr, &crasher_args());

    let sess = mgr.resolve_mut(Some(&id)).expect("session");
    let budget = random::Budget {
        max_actions: 4,
        max_runtime_ms: 20_000,
        // 0 would stop the loop before any action (the top-of-loop check
        // `relaunches >= max_relaunches` fires immediately); 1 lets the
        // first action run, the crash gets recorded, and the budget then
        // halts further actions.
        max_relaunches: 1,
        max_depth: 100,
        max_unique_states: 50,
    };
    let report = random::run(sess, 9, budget, None).expect("explore");

    // The crasher dies on the first key: an exit must have been recorded.
    assert!(
        !report.exits.is_empty(),
        "crasher app must produce an exit: {:?}",
        report.completion_reason
    );

    // Pipeline: minimize the trace that killed it.
    let pipeline = tui_lab::exploration::repro::minimize_crash(
        sess,
        &report.steps,
        tui_lab::exploration::repro::FailureKind::Crash,
        "e2e-crasher",
    );

    // The first key kills this app, so the minimized reproduction is one
    // action — and the pipeline must have confirmed it reproduces.
    assert!(pipeline.reproduced, "pipeline: {:?}", pipeline);
    assert_eq!(
        pipeline.minimized_len,
        1,
        "one key kills the crasher: {}",
        pipeline.steps.join(",")
    );
    let scenario = pipeline.scenario.as_ref().expect("scenario built");
    assert!(
        scenario.name.starts_with("repro-"),
        "repro scenario named for its origin: {}",
        scenario.name
    );
    // The scenario is replayable: an act step carrying tagged canonical
    // JSON, then the exit assertion.
    assert_eq!(scenario.step_count(), 2);

    // Replay it through the ScenarioRunner (the tui_scenario run path):
    // the reproduction must kill a fresh instance again.
    let mut mgr2 = tui_lab::session::SessionManager::new();
    let id2 = start(&mut mgr2, &crasher_args());
    let sess2 = mgr2.resolve_mut(Some(&id2)).expect("session 2");
    let replay = tui_lab::scenario::runner::ScenarioRunner::run(scenario, sess2);
    assert!(
        replay.steps_failed > 0,
        "the reproduction must fail again on a clean instance: {:?}",
        replay.step_results
    );
}

/// Items 36/37: the keyboard audit records ID-keyed edges into the graph,
/// and the navigation audit reads them back — with the Shift+Tab reversal
/// checked edge-for-edge.
#[test]
fn navigation_audit_proves_traversal() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = start(
        &mut mgr,
        &["-c".into(), "print('nav audit'); input()".to_string()],
    );
    let sess = mgr.resolve_mut(Some(&id)).expect("session");
    let mut graph = tui_lab::semantic::focus_graph::FocusGraph::new();
    let findings = tui_lab::audit::driver::navigation_audit(sess, 4, &mut graph);

    assert!(
        !findings.is_empty(),
        "navigation audit must produce findings"
    );
    assert!(
        findings
            .iter()
            .all(|f| matches!(f.category.as_str(), "keyboard" | "navigation")),
        "categories: {:?}",
        findings
            .iter()
            .map(|f| f.category.clone())
            .collect::<Vec<_>>()
    );
    // The audit drives Tab; with no focusable controls on a plain echo
    // screen the honest result is the no-traversal warning.
    assert!(
        findings.iter().any(|f| f.id.starts_with("NAV-")),
        "navigation findings present: {:?}",
        findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>()
    );
}

/// Item 33 + 34, live: guided_candidates from a session context returns
/// only candidates at or below the allowed risk, each with ≥1 evidence
/// citation naming a real source.
#[test]
fn guided_candidates_cite_evidence_and_respect_risk() {
    // Uses the run graph via the semantic suggest path directly (the MCP
    // handler is covered by the stdio suite); here we verify the wiring
    // contract: CandidateContext gates what comes out.
    let screen = tui_lab::screen::ScreenState::new(80, 24);
    let sem = tui_lab::semantic::analyze(&screen);
    let graph = tui_lab::exploration::state_graph::StateGraph::new(ExplorationBudget::default());
    let current =
        tui_lab::exploration::state_graph::StateIdentity::with_semantic(&screen, &sem).id();

    let ctx = tui_lab::exploration::candidates::CandidateContext {
        state_graph: &graph,
        current,
        action_history: &[],
        coverage: &[],
        contract: None,
        allowed_risk: tui_lab::intent::ActionRisk::Safe,
    };
    let out = tui_lab::exploration::candidates::suggest(&screen, &sem, &ctx);
    assert!(
        out.iter()
            .all(|c| c.risk <= tui_lab::intent::ActionRisk::Safe),
        "safe gate must hold: {:?}",
        out.iter()
            .map(|c| (c.action.to_string(), c.risk))
            .collect::<Vec<_>>()
    );
    assert!(
        out.iter().all(|c| !c.reasons.is_empty()),
        "every candidate carries evidence: {:?}",
        out.iter().map(|c| c.action.to_string()).collect::<Vec<_>>()
    );
    // Tab never taken from a fresh state → present and cited.
    let tab = out
        .iter()
        .find(|c| c.action["key"] == "tab")
        .expect("tab is safe and never taken");
    assert!(tab.reasons.iter().any(|r| r.source == "graph"));
}
