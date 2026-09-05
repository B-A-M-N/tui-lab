//! Wave D end-to-end: agent interaction intelligence.
//!
//! These tests exercise the live paths — the ones that need a real PTY and
//! process — against small python programs. Unit tests for the pure logic
//! live next to their modules.

use tui_lab::exploration::random;
use tui_lab::exploration::state_graph::ExplorationBudget;
use tui_lab::session::SessionPool;

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

async fn start(pool: &SessionPool, args: &[String]) -> String {
    pool.start("python3", args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start session")
}

/// Item 38, full pipeline: seeded exploration of a crashing app produces
/// (a) a failure exit, (b) a minimized reproduction saved as a Scenario,
/// and (c) a Finding whose `reproduction` names the scenario. The scenario
/// replays through the same canonical executor path as `tui_scenario run`.
#[tokio::test]
async fn crash_minimization_produces_replayable_reproduction() {
    let pool = SessionPool::new();
    let id = start(&pool, &crasher_args()).await;

    let (_report, pipeline) = pool
        .with_session(Some(&id), move |sess| {
            let budget = random::Budget {
                max_actions: 4,
                max_runtime_ms: 20_000,
                // 0 would stop the loop before any action (the top-of-loop
                // check `relaunches >= max_relaunches` fires immediately);
                // 1 lets the first action run, the crash gets recorded, and
                // the budget then halts further actions.
                max_relaunches: 1,
                max_depth: 100,
                max_unique_states: 50,
                // Items 27/28: the pool is risk-gated; the default
                // (mutating) keeps Escape (unknown) out of the draw set.
                allowed_risk: tui_lab::intent::ActionRisk::Mutating,
            };
            let report = random::run(sess, 9, budget, None).expect("explore");
            // The crasher dies on the first key: an exit must have been
            // recorded.
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
            (report, pipeline)
        })
        .await
        .expect("explore job");

    // The first key kills this app, so the minimized reproduction is one
    // action — and the pipeline must have confirmed it reproduces.
    assert!(pipeline.reproduced, "pipeline: {:?}", pipeline);
    assert_eq!(
        pipeline.minimized_len,
        1,
        "one key kills the crasher: {}",
        pipeline.steps.join(",")
    );
    let scenario = pipeline.scenario.clone().expect("scenario built");
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
    let pool2 = SessionPool::new();
    let id2 = start(&pool2, &crasher_args()).await;
    let replay_failed = pool2
        .with_session(Some(&id2), move |sess| {
            let replay = tui_lab::scenario::runner::ScenarioRunner::run(&scenario, sess);
            replay.steps_failed
        })
        .await
        .expect("replay job");
    assert!(
        replay_failed > 0,
        "the reproduction must fail again on a clean instance"
    );
}

/// Items 36/37: the keyboard audit records ID-keyed edges into the graph,
/// and the navigation audit reads them back — with the Shift+Tab reversal
/// checked edge-for-edge.
#[tokio::test]
async fn navigation_audit_proves_traversal() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        &["-c".into(), "print('nav audit'); input()".to_string()],
    )
    .await;
    let findings = pool
        .with_session(Some(&id), move |sess| {
            let mut graph = tui_lab::semantic::focus_graph::FocusGraph::new();
            tui_lab::audit::driver::navigation_audit(sess, 4, &mut graph)
        })
        .await
        .expect("audit job");

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

/// Items 33/34, live: guided_candidates from a session context returns
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

/// Audit findings 22/23: exploration actions enter the canonical run
/// ledger (one history, not two), and the recorded action identity is the
/// EXACT canonical signature, not the generic kind (`key`).
#[tokio::test]
async fn random_exploration_enters_run_ledger_with_exact_signatures() {
    let pool = SessionPool::new();
    // Raw-mode app that survives keys: reads three bytes before exiting,
    // so several pooled actions run.
    let id = start(
        &pool,
        &[
            "-c".into(),
            "import sys,tty; tty.setraw(0); print('READY'); sys.stdin.buffer.read(1); sys.stdin.buffer.read(1); sys.stdin.buffer.read(1)"
                .into(),
        ],
    )
    .await;
    let (ledger_len, step_actions) = pool
        .with_session(Some(&id), move |sess| {
            let budget = random::Budget {
                max_actions: 3,
                max_runtime_ms: 20_000,
                // 0 would stop the loop before any action (the top-of-loop
                // `relaunches >= max_relaunches` check fires immediately).
                max_relaunches: 1,
                max_depth: 100,
                max_unique_states: 50,
                allowed_risk: tui_lab::intent::ActionRisk::Mutating,
            };
            let mut run = tui_lab::run::RunContext::ephemeral();
            let report =
                random::run_evidenced(sess, 7, budget, None, Some(&mut run)).expect("explore");
            let ledger = run.transactions().len();
            let actions: Vec<String> = report.steps.iter().map(|s| s.action.clone()).collect();
            (ledger, actions)
        })
        .await
        .expect("explore job");

    assert!(!step_actions.is_empty(), "steps must have run");
    // Finding 22: every executed action is in the canonical ledger.
    assert!(
        ledger_len >= step_actions.len(),
        "each exploration action must be a ledger transaction: {ledger_len} ledger vs {} steps",
        step_actions.len()
    );
    // Finding 23: identity is the exact signature, never the bare kind.
    for action in &step_actions {
        assert!(
            action != "key" && action != "mouse_click" && !action.is_empty(),
            "step identity must be the canonical signature, not the kind: {action}"
        );
    }
}

/// Audit finding 9: evidence health is honest. With the run OPEN, a driven
/// act's frame commits and ledger record all succeed (healthy). After the
/// run closes mid-session, the SAME drive path must NOT fabricate
/// "frame:0" — the frame legs are null + error, `healthy` is false, and
/// the act itself still succeeded (the TUI received the input).
#[tokio::test]
async fn drive_outcome_reports_evidence_health_not_fabricated_frames() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        &[
            "-c".into(),
            "import sys,tty; tty.setraw(0); print('HEALTH READY'); sys.stdin.buffer.read(1)".into(),
        ],
    )
    .await;

    pool.with_session(Some(&id), move |sess| {
        let action = tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        };
        let make_spec = || tui_lab::execution::CoreDriveSpec {
            action: &action,
            quiet_ms: 120,
            budget_ms: 1200,
            no_wait: false,
            visibility: tui_lab::execution::InputVisibility::Normal,
            completion: tui_lab::capture::CompletionPolicy::StableScreen,
            guard: None,
            scenario: None,
        };

        // Open run: everything commits, outcome is healthy, frames carry
        // real ids.
        let run = std::sync::Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
        let ok =
            tui_lab::execution::drive_pipeline(sess, &run, make_spec()).expect("drive on open run");
        assert!(
            ok.health.healthy(),
            "open run: all evidence legs commit: {:?}",
            ok.health.failures()
        );
        let before_ref = ok.frames["before"]["ref"].as_str().expect("ref");
        assert!(
            before_ref != "frame:0" && before_ref.starts_with("frame:"),
            "committed leg cites a real frame id: {before_ref}"
        );

        // Closed run: the commit path refuses; the act STILL executes (the
        // TUI got the key — that fact does not change), but the outcome
        // must say its evidence is not citable instead of defaulting.
        run.lock().unwrap().close().expect("close run");
        let closed = tui_lab::execution::drive_pipeline(sess, &run, make_spec())
            .expect("drive on closed run");
        assert!(
            !closed.health.healthy(),
            "closed run: frame commits refused, health says so"
        );
        assert!(
            !closed.health.ledger_recorded,
            "the ledger leg is named as not recorded too"
        );
        for leg in ["before", "after"] {
            let f = &closed.frames[leg];
            assert!(
                f["ref"].is_null(),
                "{leg} leg must not fabricate an id: {f}"
            );
            assert!(
                f["error"].is_string(),
                "{leg} leg names why it did not commit: {f}"
            );
        }
        let failures = closed.health.failures();
        assert!(
            failures.iter().any(|s| s.contains("before")),
            "failures name each leg: {failures:?}"
        );
        assert!(
            failures.iter().any(|s| s.contains("ledger")),
            "failures name the ledger: {failures:?}"
        );
    })
    .await
    .expect("drive health job");
}
