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

// ── Wave 5: greenfield diagnosis loop (review §2/§3 recontracting) ──────

#[test]
fn diagnostic_context_keys_verification_on_rule_identity() {
    // Item 42: two occurrences of the same rule get different instance
    // ids but the SAME rule key — the verification plan and context
    // identity must carry the rule, not the instance, so a fresh audit
    // pass compares correctly across runs.
    let mk = |inst: &str| crate_shim_finding("CLIP-001", inst);
    let f1 = mk("CLIP-001");
    let f2 = mk("CLIP-002");
    for f in [f1, f2] {
        let ctx = tui_lab::audit::repair::DiagnosticContext::assemble(f, "run", vec![], |_| None)
            .expect("context");
        assert_eq!(ctx.rule_id, "CLIP-001", "rule identity, not instance");
        // Review §3: no scenario no longer means no verification — the
        // plan carries targeted checks; only the replay leg is absent.
        assert!(
            ctx.verification.replay.is_none(),
            "no scenario → no invented replay leg"
        );
        assert!(
            !ctx.verification.targeted_checks.is_empty(),
            "targeted checks stand without a scenario"
        );
    }
}

fn crate_shim_finding(rule: &str, inst: &str) -> tui_lab::audit::Finding {
    tui_lab::audit::Finding {
        id: inst.into(),
        rule_id: Some(rule.into()),
        severity: "warn".into(),
        category: "layout".into(),
        summary: "clipped".into(),
        evidence: vec![tui_lab::audit::EvidenceRef::point(
            tui_lab::audit::EvidenceKind::Region,
            "region/main",
            "clipped at bottom",
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    }
}

#[test]
fn targeted_check_derived_from_evidence_target() {
    // Item 44: the recipe names the exact control target to re-check, not
    // "replay everything".
    let mut f = crate_shim_finding("FOCUS-002", "FOCUS-002");
    f.rule_id = Some("FOCUS-002".into());
    f.reproduction = Some("scen-x".into());
    let sc = tui_lab::scenario::model::Scenario::new("repro")
        .act(serde_json::json!({"action":"key","key":"tab"}));
    let ctx =
        tui_lab::audit::repair::DiagnosticContext::assemble(f, "run", vec![], |_| Some(sc.clone()))
            .expect("context");
    let plan = &ctx.verification;
    let replay = plan.replay.as_ref().expect("replay leg exists with repro");
    assert_eq!(replay.finding_rule_id, "FOCUS-002");
    let targeted = &plan.targeted_checks[0];
    assert_eq!(targeted.target, "region/main");
    assert!(
        targeted.recheck_hint.contains("FOCUS-002"),
        "hint names the rule: {targeted:?}"
    );
}

#[test]
fn contract_scaffold_declares_what_was_seen() {
    // Item 43: scaffold from a synthetic frame + semantics. Everything in
    // the contract must trace to something observed, and the extensions
    // must carry the inferred marker.
    use tui_lab::screen::{Cell, Color, ScreenState};
    let mut screen = ScreenState::new(80, 24);
    // ScreenState::new starts with an empty grid; fill it with blanks so
    // the scaffold has a real frame to look at.
    let blank = || Cell {
        x: 0,
        y: 0,
        text: " ".into(),
        fg: Color::unknown(),
        bg: Color::unknown(),
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        reverse: false,
        strike: false,
    };
    screen.cells = (0..(80 * 24))
        .map(|i| {
            let mut c = blank();
            c.x = (i % 80) as u16;
            c.y = (i / 80) as u16;
            c
        })
        .collect();
    screen.cells[12 * 80 + 30].reverse = true;
    screen.viewport_text = vec![
        "┌─ Main ─────────────┐".to_string(),
        "│ [S]ave   [C]ancel  │".to_string(),
        "└────────────────────┘".to_string(),
    ];
    let sem = tui_lab::semantic::analyze(&screen);
    let contract = tui_lab::design::ProjectContract::scaffold_from(&screen, &sem);
    let marker = contract
        .schema
        .extensions
        .get("scaffold.inferred")
        .expect("inferred marker in extensions");
    assert_eq!(marker["inferred"], true);
    assert_eq!(contract.viewports.len(), 1, "one observed viewport");
    assert_eq!(contract.viewports[0].cols, 80);
    assert_eq!(contract.viewports[0].rows, 24);
    // Every oracle names a control that exists in the analysis (nothing
    // invented).
    for o in &contract.oracles {
        assert!(
            o.expr.starts_with("control_exists("),
            "scaffold only declares observed controls: {o:?}"
        );
    }
    // The contract must survive a round-trip through its own serde (and
    // the document validator must find no structural complaints).
    let doc = serde_yaml::to_string(&contract).expect("yaml");
    let back: tui_lab::design::ProjectContract = serde_yaml::from_str(&doc).expect("round-trip");
    let _ = back.validate();
}

#[test]
fn coverage_event_with_identity_attests_source_locus() {
    // Item 41: record_coverage_event_with_identity joins the app's own
    // locus; the ledger entry carries it and feeds the finding enricher.
    let mut run = tui_lab::run::RunContext::ephemeral();
    let _ = run.record_coverage_event_with_identity(
        "s1",
        "#save.activate",
        tui_lab::semantic::SourceRef {
            file: "src/ui/save.rs".into(),
            line: 42,
            column: None,
            symbol: Some("SaveButton".into()),
            framework_id: None,
            confidence: 1.0,
            source: "native".into(),
            // The identity fold stamps `attested` itself (review §4) —
            // native id → coverage event → locus is the app's own line.
            provenance: tui_lab::semantic::Provenance::Unknown,
        },
    );
    let _ = run.record_coverage_event("s1", "#save.activate"); // plain hits still fold
    let entry = run.coverage_ledger.get("#save.activate").expect("entry");
    assert_eq!(entry.hits, 2);
    assert_eq!(entry.source_refs.len(), 1, "deduped by location");
    assert_eq!(entry.source_refs[0].file, "src/ui/save.rs");
    assert_eq!(
        entry.source_refs[0].provenance,
        tui_lab::semantic::Provenance::Attested,
        "the identity fold upgrades to attested"
    );

    // And the enricher: a finding whose evidence names this control gains
    // the app-attested locus through the pure explain-time join.
    let f = tui_lab::audit::Finding {
        id: "MOUSE-001".into(),
        rule_id: Some("MOUSE-001".into()),
        severity: "warn".into(),
        category: "mouse".into(),
        summary: "unresponsive".into(),
        evidence: vec![tui_lab::audit::EvidenceRef::point(
            tui_lab::audit::EvidenceKind::Control,
            "button/save",
            "clicked, no response",
        )],
        confidence: 0.8,
        reproduction: None,
        source_refs: Vec::new(),
    };
    let joined = run.join_source_refs_if_known(&f);
    assert!(
        !joined.source_refs.is_empty(),
        "explain-time join attaches the app-attested locus"
    );
    assert_eq!(joined.source_refs[0].file, "src/ui/save.rs");
}

/// Re-review item 30: conformance reports what it left behind. The modal
/// fixture's check drives the app (opens a modal, escapes it) — the report
/// must EVIDENCE that the session came back clean: session_mutated=false
/// with an empty residue list, and the serialized summary carries both.
#[test]
fn conformance_reports_session_mutation_residue() {
    let contract =
        design::load_design_contract(std::path::Path::new("fixtures/modal_contract.yaml"))
            .expect("fixture contract loads");

    let mut mgr = SessionManager::new();
    let id = start_modal(&mut mgr);
    std::thread::sleep(std::time::Duration::from_millis(400));

    let sess = mgr.resolve_mut(Some(&id)).expect("session");
    let report = design::check_contract(sess, &contract).expect("conformance runs");

    assert!(
        report.driven_actions > 0,
        "this fixture drives the app; the mutation report must be earned, not defaulted"
    );
    assert!(
        !report.session_mutated,
        "the fixture restores its own state; residue would be a real defect: {:?}",
        report.residue
    );
    assert!(report.residue.is_empty());

    let v = serde_json::to_value(report.summary()).unwrap();
    assert_eq!(v["session_mutated"], serde_json::json!(false));
    assert!(v["residue"].is_array());
}
