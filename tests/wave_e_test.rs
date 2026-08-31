//! Wave E end-to-end: construction-layer contracts.
//!
//! These tests exercise the live paths against a real PTY: contract loading
//! (real YAML), normalization-policy application (item 48), conformance
//! checking that drives the app (items 45–47), and contract-fed exploration
//! (item 49). Pure-logic unit tests live next to their modules.

use tui_lab::design;
use tui_lab::session::SessionManager;

/// Launch the modal fixture. Raw-mode python so keys deliver immediately.
fn start_modal(mgr: &mut SessionManager) -> String {
    mgr.start(
        "python3",
        &["fixtures/modal_tui.py".to_string()],
        None,
        &[],
        60,
        9,
        "auto",
        "local",
    )
    .expect("start modal fixture")
}

/// Item 39/44: the shipped fixture contract parses through the real YAML
/// loader, and its oracle expressions all validate.
#[test]
fn fixture_contract_parses_and_validates() {
    let contract =
        design::load_design_contract(std::path::Path::new("fixtures/modal_contract.yaml"))
            .expect("fixture contract loads");
    assert_eq!(contract.schema.name, "modal-fixture");
    assert_eq!(contract.components.len(), 1);
    assert_eq!(contract.interactions.len(), 2);
    assert_eq!(contract.layout.len(), 1);
    assert_eq!(contract.oracles.len(), 1);

    let results = design::validate_document(&contract);
    assert!(
        results.iter().all(|r| r.verdict != design::Verdict::Fail),
        "document must validate: {:?}",
        results
            .iter()
            .filter(|r| r.verdict == design::Verdict::Fail)
            .collect::<Vec<_>>()
    );
}

/// Item 48: loading a contract's normalization policy changes structure
/// hashes — a declared-volatile token stops fragmenting the hash.
#[test]
fn volatile_pattern_policy_changes_structure_hash() {
    use tui_lab::screen::NormalizationPolicy;

    // A row with a clock. Default policy already collapses clocks, so use a
    // CUSTOM token class only the contract would declare.
    let mut parser = vt100::Parser::new(3, 30, 0);
    parser.process(b"job 42 running");

    let default_hash =
        tui_lab::screen::from_vt(parser.screen(), process_state(), None, Vec::new()).structure_hash;

    // Contract says bare job IDs are volatile.
    let policy = tui_lab::screen::normalize::from_patterns(&[r"\bjob \d+\b".to_string()])
        .expect("pattern compiles");
    let policy_hash = tui_lab::screen::from_vt_with_policy(
        parser.screen(),
        process_state(),
        None,
        Vec::new(),
        &policy,
    )
    .structure_hash;

    assert_ne!(
        default_hash, policy_hash,
        "the contract-declared volatile class must change the structure hash"
    );

    // Sanity: the default policy hash is stable across calls, and a policy
    // holding the SAME patterns the row matches must equal itself.
    let policy2 = tui_lab::screen::normalize::from_patterns(&[r"\bjob \d+\b".to_string()]).unwrap();
    let policy_hash2 = tui_lab::screen::from_vt_with_policy(
        parser.screen(),
        process_state(),
        None,
        Vec::new(),
        &policy2,
    )
    .structure_hash;
    assert_eq!(policy_hash, policy_hash2);

    // A policy that normalizes "42" too must produce a DIFFERENT hash from
    // the job-only policy (patterns are part of hash identity).
    let broader = tui_lab::screen::normalize::from_patterns(&[r"\d+".to_string()]).unwrap();
    let broader_hash = tui_lab::screen::from_vt_with_policy(
        parser.screen(),
        process_state(),
        None,
        Vec::new(),
        &broader,
    )
    .structure_hash;
    assert_ne!(broader_hash, policy_hash);
    let _: NormalizationPolicy = broader; // type anchor
}

fn process_state() -> tui_lab::screen::ProcessState {
    tui_lab::screen::ProcessState {
        running: true,
        exit_code: None,
        exit_signal: None,
        cwd: None,
        pid: None,
    }
}

/// Items 45–47, live: conformance against the modal fixture. The
/// interaction oracle "n → modal_open()" only passes if the app REALLY
/// opened a modal, and "escape → modal_open(false)" only if Escape REALLY
/// closed it — the report is earned, not assumed.
#[test]
fn conformance_drives_the_modal_fixture() {
    let contract =
        design::load_design_contract(std::path::Path::new("fixtures/modal_contract.yaml"))
            .expect("fixture contract loads");

    let mut mgr = SessionManager::new();
    let id = start_modal(&mut mgr);
    // Give the fixture a moment to draw its first frame.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let sess = mgr.resolve_mut(Some(&id)).expect("session");
    let report = design::check_contract(sess, &contract).expect("conformance runs");

    // The report must name every group it checked.
    let groups: std::collections::HashSet<&str> = report.results.iter().map(|r| r.group).collect();
    assert!(groups.contains("document"), "groups: {:?}", groups);
    assert!(groups.contains("component"), "groups: {:?}", groups);
    assert!(groups.contains("interaction"), "groups: {:?}", groups);
    assert!(groups.contains("layout"), "groups: {:?}", groups);
    assert!(groups.contains("behavior"), "groups: {:?}", groups);

    // The app really was driven.
    assert!(
        report.driven_actions > 0,
        "conformance must have sent keys: {:?}",
        report.summary()
    );

    // The open-confirm-modal interaction (n → modal) and the Escape
    // behavior check must both pass against this fixture.
    let open = report
        .results
        .iter()
        .find(|r| r.group == "interaction" && r.name == "open-confirm-modal")
        .expect("interaction result present");
    assert_eq!(
        open.verdict,
        design::Verdict::Pass,
        "n must open the modal: {}",
        open.detail
    );
    let esc = report
        .results
        .iter()
        .find(|r| r.group == "behavior" && r.name == "escape_closes_modal")
        .expect("escape behavior result present");
    assert_eq!(
        esc.verdict,
        design::Verdict::Pass,
        "Escape must close the modal (proven): {}",
        esc.detail
    );

    // Failed conformance folds into findings with contract/ categories.
    let findings = report.findings();
    if report.verdict != design::Verdict::Pass {
        assert!(
            findings.iter().any(|f| f.category.starts_with("contract/")),
            "non-pass reports must carry findings"
        );
    }
}

/// Item 49: with a contract loaded, exploration candidates cite it.
#[test]
fn loaded_contract_feeds_candidate_evidence() {
    use tui_lab::exploration::candidates::{suggest, CandidateContext};
    use tui_lab::exploration::state_graph::{ExplorationBudget, StateGraph};

    let contract = design::parse_yaml(
        "schema:\n  name: t\n  version: \"1\"\nkeybindings:\n  - action: quit\n    keys: [\"Q\"]\n",
    )
    .expect("contract parses");

    let screen = tui_lab::screen::ScreenState {
        cols: 40,
        rows: 2,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["hello".into(), "world".into()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: "s".into(),
        process: process_state(),
    };
    let sem = tui_lab::semantic::analyze(&screen);
    let g = StateGraph::new(ExplorationBudget::default());
    let ctx = CandidateContext {
        state_graph: &g,
        current: tui_lab::exploration::state_graph::StateId::from_structure_hash("s"),
        action_history: &[],
        coverage: &[],
        contract: Some(&contract),
        allowed_risk: tui_lab::intent::ActionRisk::Mutating,
    };
    let out = suggest(&screen, &sem, &ctx);
    let q = out
        .iter()
        .find(|c| c.action["key"] == "Q")
        .expect("declared-but-unexercised key becomes a candidate");
    assert!(
        q.reasons.iter().any(|r| r.source == "contract"),
        "reason must cite the contract: {:?}",
        q.reasons
    );
}

/// Oracle in a scenario: the replay path evaluates `assertion: "oracle"`
/// steps through the shared language.
#[test]
fn scenario_oracle_steps_replay() {
    let mut mgr = SessionManager::new();
    let _id = start_modal(&mut mgr);
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Build a scenario: press n, then the oracle "modal_open()".
    let mut recorder = tui_lab::scenario::recorder::ScenarioRecorder::new("oracle-check");
    recorder.record_act(serde_json::json!({"action": "key", "key": "n"}));
    recorder.record_assert(serde_json::json!({"assertion": "oracle", "text": "modal_open()"}));
    recorder.record_assert(
        serde_json::json!({"assertion": "oracle", "text": "text_present(\"Add connection?\")"}),
    );
    let scenario = recorder.build();

    let sess = mgr.resolve_mut(None).expect("session");
    let report = tui_lab::scenario::ScenarioRunner::run(&scenario, sess);
    assert_eq!(report.steps_total, 3);
    assert_eq!(
        report.steps_failed,
        0,
        "oracle steps must pass against the real modal: {:?}",
        report
            .step_results
            .iter()
            .map(|r| (r.index, r.detail.clone()))
            .collect::<Vec<_>>()
    );
}

/// A failing oracle fails the scenario honestly (the regression value).
#[test]
fn failing_oracle_fails_scenario_step() {
    let mut mgr = SessionManager::new();
    let _id = start_modal(&mut mgr);
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut recorder = tui_lab::scenario::recorder::ScenarioRecorder::new("oracle-fail");
    recorder.record_assert(serde_json::json!({"assertion": "oracle", "text": "modal_open()"}));
    let scenario = recorder.build();

    let sess = mgr.resolve_mut(None).expect("session");
    let report = tui_lab::scenario::ScenarioRunner::run(&scenario, sess);
    assert_eq!(report.steps_failed, 1, "no modal is open: {:?}", report);
    assert!(
        report.step_results[0].detail.contains("modal"),
        "failure detail names the modal evidence: {}",
        report.step_results[0].detail
    );
}
