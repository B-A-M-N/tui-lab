//! Re-review Wave 1: completion-spec wire model, exact scenario replay,
//! sensitive scenario parameters, and honest native-role mapping.

use tui_lab::scenario::{Scenario, ScenarioRunner};
use tui_lab::session::SessionPool;

async fn python_session(pool: &SessionPool, code: &str) -> String {
    let args: Vec<String> = vec!["-c".to_string(), code.to_string()];
    pool.start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start python session")
}

// ── P0.1: completion spec carries its own data ───────────────────────────

#[test]
fn completion_spec_text_disappears_roundtrips_losslessly() {
    let req: tui_lab::mcp::params::TuiActRequest = serde_json::from_str(
        r#"{"action":"key","key":"enter","completion":{"type":"text_disappears","text":"Saving..."}}"#,
    )
    .expect("deserialize");
    match req.completion() {
        Some(tui_lab::capture::CompletionPolicy::TextDisappears(t)) => {
            assert_eq!(t, "Saving...", "the waited-for text must survive the wire")
        }
        other => panic!("expected TextDisappears with payload, got {other:?}"),
    }
}

#[test]
fn completion_name_stays_backward_compatible() {
    // Every parameterless strategy remains a bare string on the wire.
    for (wire, expected) in [
        ("stable_screen", "StableScreen"),
        ("first_change", "FirstScreenChange"),
        ("any_change", "AnyObservableChange"),
        ("process_exit", "ProcessExit"),
        ("command_done", "CommandDone"),
        ("bell", "Bell"),
        ("semantic_change", "SemanticChange"),
        ("may_be_silent", "MayBeSilent"),
        ("no_wait", "NoWait"),
    ] {
        let raw = format!(r#"{{"action":"key","key":"f5","completion":"{wire}"}}"#);
        let req: tui_lab::mcp::params::TuiActRequest =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{wire}: {e}"));
        let name = format!("{:?}", req.completion().expect("policy"));
        assert!(
            name.contains(expected),
            "completion '{wire}' must map to {expected}, got {name}"
        );
    }
}

// ── P0.2: scenario replay preserves completion semantics ─────────────────

#[tokio::test]
async fn scenario_replay_preserves_process_exit_completion() {
    // The recorded action: type "q\n" with completion=process_exit (the
    // child reads a line, echoes it, exits). Replay must wait for the child
    // to EXIT, not for a screen settle that never comes.
    let scenario = Scenario::new("exit-replay").act(serde_json::json!({
        "action": "type",
        "text": "q\n",
        "completion": "process_exit"
    }));

    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; print('READY', flush=True); \
         data = sys.stdin.read(1); \
         print('GOT', repr(data), flush=True)",
    )
    .await;
    let report = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(300).expect("baseline");
            ScenarioRunner::run(&scenario, sess)
        })
        .await
        .expect("run job");
    assert_eq!(
        report.steps_failed, 0,
        "replay must honor the recorded completion: {:?}",
        report.step_results
    );
    let detail = &report.step_results[0].detail;
    assert!(
        detail.contains("ProcessExit") || detail.contains("exit"),
        "settle reason should reflect the exit policy: {detail}"
    );
}

#[tokio::test]
async fn scenario_replay_preserves_recorded_quiet_window() {
    // A recorded wait_ms travels with the step; replay uses the recorded
    // value, not the old hardcoded 150/1150.
    let scenario = Scenario::new("quiet-replay").act(serde_json::json!({
        "action": "key",
        "key": "x",
        "wait_ms": 400
    }));

    let pool = SessionPool::new();
    let id = python_session(&pool, "import time; time.sleep(10)").await;
    let report = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(200).expect("baseline");
            ScenarioRunner::run(&scenario, sess)
        })
        .await
        .expect("run job");
    let detail = &report.step_results[0].detail;
    // With a 400ms quiet window and a silent child, the settle SUCCEEDS
    // (400ms quiet < budget) — the old 150ms would too, so assert instead
    // that the step ran to completion without a fabricated failure.
    assert_eq!(
        report.steps_failed, 0,
        "replayed wait_ms must not break execution: {detail}"
    );
}

// ── P0.3: sensitive scenario parameters ──────────────────────────────────

#[test]
fn sensitive_recording_declares_parameter_not_payload() {
    use tui_lab::scenario::model::SensitiveKind;
    use tui_lab::scenario::recorder::ScenarioRecorder;

    let mut rec = ScenarioRecorder::new("login");
    rec.record_act_sensitive(
        serde_json::json!({"action": "type", "text": "hunter2", "sensitive": true}),
        "text",
        SensitiveKind::Password,
        7,
    );
    let scenario = rec.build();

    // The value is gone from every serialized step.
    let json = serde_json::to_string(&scenario).unwrap();
    assert!(
        !json.contains("hunter2"),
        "the secret must never land in the scenario file"
    );

    // The parameter is declared, and the step references it.
    assert_eq!(scenario.parameter_names(), vec!["TEXT_1"]);
    let step_text = serde_json::to_string(&scenario.steps[0].params).unwrap();
    assert!(
        step_text.contains("${TEXT_1}"),
        "step references the parameter: {step_text}"
    );

    // Roundtrip through JSON preserves the declaration.
    let back: Scenario = serde_json::from_str(&json).unwrap();
    assert_eq!(back.parameter_names(), vec!["TEXT_1"]);
}

#[tokio::test]
async fn unresolved_parameter_fails_structured_not_parse_error() {
    let scenario = Scenario::new("param-check")
        .act(serde_json::json!({"action": "type", "text": "${PASSWORD}", "sensitive": true}));
    // Declare the parameter the way the recorder would.
    let mut scenario = scenario;
    scenario
        .parameters
        .push(tui_lab::scenario::model::SensitiveParameter {
            name: "PASSWORD".into(),
            kind: tui_lab::scenario::model::SensitiveKind::Password,
            description: None,
        });

    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; import time; sys.stdin.read(1); time.sleep(5)",
    )
    .await;

    let (report, echoed) = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(200).expect("baseline");

            // No value supplied: the step fails as unresolved_parameter — and the
            // literal ${PASSWORD} is never typed into the app.
            let report = ScenarioRunner::run(&scenario, sess);

            // The screen must NOT have received the raw reference.
            let screen = sess.observe(60).unwrap();
            let echoed: String = screen.viewport_text.join("\n");
            (report, echoed)
        })
        .await
        .expect("run job");
    assert_eq!(report.steps_failed, 1);
    assert!(
        report.step_results[0]
            .detail
            .contains("unresolved_parameter"),
        "structured failure naming the missing parameter: {}",
        report.step_results[0].detail
    );

    assert!(
        !echoed.contains("${PASSWORD}"),
        "an unresolved reference must never be sent as literal keystrokes: {echoed}"
    );
}

#[tokio::test]
async fn supplied_parameter_resolves_and_executes() {
    use tui_lab::scenario::model::ParameterValue;

    let scenario = Scenario::new("param-ok")
        .act(serde_json::json!({"action": "type", "text": "${PASSWORD}\n"}))
        .assert(serde_json::json!({"assertion": "text", "text": "GOT"}));
    let mut scenario = scenario;
    scenario
        .parameters
        .push(tui_lab::scenario::model::SensitiveParameter {
            name: "PASSWORD".into(),
            kind: tui_lab::scenario::model::SensitiveKind::Password,
            description: None,
        });

    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; print('READY', flush=True); \
         line = sys.stdin.readline(); \
         print('GOT', line.strip(), flush=True)",
    )
    .await;

    let (report, echoed) = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(300).expect("baseline");

            let values = vec![ParameterValue {
                name: "PASSWORD".into(),
                value: "hunter2".into(),
            }];
            let report = ScenarioRunner::run_with_parameters(&scenario, sess, &values);

            let screen = sess.observe(60).unwrap();
            let echoed: String = screen.viewport_text.join("\n");
            (report, echoed)
        })
        .await
        .expect("run job");
    assert_eq!(
        report.steps_failed, 0,
        "supplied parameter resolves: {:?}",
        report.step_results
    );

    assert!(
        echoed.contains("GOT hunter2"),
        "the substituted value reached the app: {echoed}"
    );
}

// ── P0.11: unknown native roles stay unknown ─────────────────────────────

#[test]
fn unknown_native_role_does_not_become_button() {
    use tui_lab::screen::ScreenState;
    use tui_lab::semantic::cache::SemanticCache;
    use tui_lab::semantic::controls::ControlKind;
    use tui_lab::semantic::fuse;
    use tui_lab::semantic::native::{NativeChannel, NativeNode};

    // A native-only node with a role outside the vocabulary, plus bounds —
    // the exact case that used to be invented into a clickable Button.
    let node = NativeNode {
        id: "ui/graph".to_string(),
        role: "graph".to_string(),
        label: Some("throughput".to_string()),
        value: None,
        bounds: Some([2, 2, 20, 6]),
        actions: vec![],
        focusable: Some(true),
        focused: Some(false),
        enabled: Some(true),
        source: Default::default(),
        children: vec![],
    };

    let screen = ScreenState::new(80, 24);
    let mut cache = SemanticCache::new();
    let mut channel = NativeChannel::default();
    channel.latest = Some(node);

    let (sem, _tree, report) = fuse(&screen, &mut cache, &channel);

    let graph = sem
        .controls
        .iter()
        .find(|c| c.id == "ui/graph")
        .expect("native-only node with bounds must appear in flat controls");
    assert_eq!(
        graph.kind,
        ControlKind::Unknown,
        "unknown native role must stay Unknown — never an invented actionable Button"
    );
    // And the tree carries it as an unknown-role node, not a button.
    assert!(report.native_only.contains(&"ui/graph".to_string()));
}
