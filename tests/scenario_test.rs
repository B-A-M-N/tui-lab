// Tests for the scenario recorder/runner/replay system (spec item 39).

use tui_lab::scenario::{Scenario, ScenarioMetadata, ScenarioRecorder, ScenarioRunner, StepKind};

#[test]
fn scenario_model_new() {
    let scenario = Scenario::new("test-scenario");
    assert_eq!(scenario.name, "test-scenario");
    assert_eq!(scenario.schema, "tui-lab/scenario/v1");
    assert!(scenario.steps.is_empty());
    assert!(!scenario.is_valid());
}

#[test]
fn scenario_model_add_steps() {
    let scenario = Scenario::new("test")
        .act(serde_json::json!({"action": "key", "key": "tab"}))
        .wait(serde_json::json!({"condition": "text", "text": "Settings"}))
        .assert(serde_json::json!({"assertion": "text", "text": "Settings"}));

    assert_eq!(scenario.step_count(), 3);
    assert!(scenario.is_valid());
    assert_eq!(scenario.steps[0].kind, StepKind::Act);
    assert_eq!(scenario.steps[1].kind, StepKind::Wait);
    assert_eq!(scenario.steps[2].kind, StepKind::Assert);
}

#[test]
fn scenario_model_with_metadata() {
    let mut scenario = Scenario::new("test");
    scenario.metadata = Some(ScenarioMetadata {
        description: Some("A test scenario".into()),
        tags: vec!["regression".into(), "smoke".into()],
        created_at: None,
        source: None,
    });

    let json = serde_json::to_string(&scenario).unwrap();
    assert!(json.contains("A test scenario"));
    assert!(json.contains("regression"));
}

#[test]
fn scenario_model_roundtrip() {
    let scenario = Scenario::new("roundtrip")
        .act(serde_json::json!({"action": "key", "key": "enter"}))
        .assert(serde_json::json!({"assertion": "text", "text": "Hello"}));

    let json = serde_json::to_string(&scenario).unwrap();
    let deserialized: Scenario = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.name, "roundtrip");
    assert_eq!(deserialized.step_count(), 2);
}

#[test]
fn scenario_recorder_builds_scenario() {
    let mut recorder = ScenarioRecorder::new("recorded-test");
    recorder.record_act(serde_json::json!({"action": "key", "key": "tab"}));
    recorder.record_wait(serde_json::json!({"condition": "screen_stable"}));
    recorder.record_assert(serde_json::json!({"assertion": "text", "text": "Done"}));

    let scenario = recorder.build();
    assert_eq!(scenario.name, "recorded-test");
    assert_eq!(scenario.step_count(), 3);
    assert!(scenario.is_valid());
}

#[tokio::test]
async fn scenario_runner_executes_steps() {
    // The runner must actually EXECUTE steps through the canonical executor:
    // the act's keystroke must reach the child and the assert must run
    // against the real screen — not the old fabricated "(true, act step
    // executed)" placeholders.
    let scenario = Scenario::new("runner-test")
        .act(serde_json::json!({"action": "type", "text": "echo hi"}))
        .wait(serde_json::json!({"condition": "screen_stable", "budget_ms": 2000}))
        .assert(serde_json::json!({"assertion": "text", "text": "test"}));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "print('test'); import time; time.sleep(10)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let report = pool
        .with_session(Some(&id), move |sess| ScenarioRunner::run(&scenario, sess))
        .await
        .expect("run job");

    assert_eq!(report.scenario_name, "runner-test");
    assert_eq!(report.steps_total, 3);
    assert_eq!(
        report.steps_failed, 0,
        "all steps must pass against the real session: {:?}",
        report.step_results
    );
    // The act step detail must show real execution, not the old placeholder.
    assert!(
        report.step_results[0]
            .detail
            .contains("act executed, settled="),
        "act detail: {}",
        report.step_results[0].detail
    );
}

#[tokio::test]
async fn scenario_runner_detects_failures() {
    // A failing assertion must produce a real failure with real detail.
    let scenario = Scenario::new("fail-test")
        .act(serde_json::json!({"action": "type", "text": "hello"}))
        .assert(serde_json::json!({"assertion": "text", "text": "never_matches"}));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "print('test'); import time; time.sleep(10)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let report = pool
        .with_session(Some(&id), move |sess| ScenarioRunner::run(&scenario, sess))
        .await
        .expect("run job");

    assert_eq!(report.steps_total, 2);
    assert_eq!(
        report.steps_passed, 1,
        "act passes: {:?}",
        report.step_results
    );
    assert_eq!(
        report.steps_failed, 1,
        "assert must fail for real: {:?}",
        report.step_results
    );
    assert!(
        report.step_results[1].detail.contains("never_matches"),
        "assert detail must state the expectation: {}",
        report.step_results[1].detail
    );
}

#[tokio::test]
async fn scenario_runner_actually_sends_input() {
    // The strongest form of the re-review item-4 requirement: a scenario
    // that types text must make that text appear in the child's terminal.
    let scenario =
        Scenario::new("type-test").act(serde_json::json!({"action": "type", "text": "marker-xyz"}));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec!["-c".into(), "import time; time.sleep(10)".into()];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let report = pool
        .with_session(Some(&id), move |sess| ScenarioRunner::run(&scenario, sess))
        .await
        .expect("run job");
    assert_eq!(report.steps_failed, 0, "{:?}", report.step_results);

    let echoed = pool
        .with_session(Some(&id), |sess| {
            sess.observe(40)
                .map(|screen| {
                    screen
                        .viewport_text
                        .iter()
                        .any(|r| r.contains("marker-xyz"))
                })
                .expect("observe")
        })
        .await
        .expect("observe job");
    assert!(
        echoed,
        "typed text must reach the PTY (terminal echo)"
    );
}

// ── Wave-2: mutation guards (StepExpect) ─────────────────────────────────

/// A guarded step whose recorded structure hash no longer matches the live
/// screen fails with `stale_state` — and the input is NOT sent (the child
/// sees nothing). The old behavior sent the keystroke into whatever UI
/// happened to be up.
#[tokio::test]
async fn stale_guard_blocks_input_on_drift() {
    use tui_lab::scenario::model::StepExpect;

    // Child prints READY, then blocks on stdin: a line is only visible if
    // input actually arrived.
    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "print('GUARD-READY'); import sys; sys.stdin.readline(); print('GUARD-GOT-INPUT'); import time; time.sleep(5)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
        sess.observe(300).expect("baseline");
    })
    .await
    .expect("baseline job");

    // A captured structure hash that CANNOT match (capture happened on a
    // different layout — the drift simulation).
    let step = tui_lab::scenario::model::ScenarioStep {
        kind: tui_lab::scenario::model::StepKind::Act,
        params: serde_json::json!({"action": "type", "text": "should not land\n"}),
        expect: Some(StepExpect {
            structure_hash: Some("definitely-not-the-live-hash".into()),
            focus_control_id: None,
            text_present: None,
        }),
    };
    let scenario = tui_lab::scenario::model::Scenario {
        steps: vec![step],
        ..tui_lab::scenario::model::Scenario::new("guard-test")
    };

    let report = pool
        .with_session(Some(&id), move |sess| ScenarioRunner::run(&scenario, sess))
        .await
        .expect("run job");
    assert_eq!(
        report.steps_failed, 1,
        "guard must fail: {:?}",
        report.step_results
    );
    assert!(
        report.step_results[0].detail.contains("stale_state"),
        "verdict names the drift: {}",
        report.step_results[0].detail
    );
    assert!(
        report.step_results[0].detail.contains("structure drifted"),
        "verdict names the condition: {}",
        report.step_results[0].detail
    );

    // Prove the input never landed: give the child a moment, then check
    // GUARD-GOT-INPUT is absent from any frame.
    let leaked = pool
        .with_session(Some(&id), |sess| {
            sess.observe(300).expect("post check");
            sess.last()
                .expect("frame")
                .viewport_text
                .iter()
                .any(|r| r.contains("GUARD-GOT-INPUT"))
        })
        .await
        .expect("post-check job");
    assert!(
        !leaked,
        "guarded input must NOT reach the app"
    );
    pool.stop(&id).await.ok();
}

/// A guard that matches lets the step run normally.
#[tokio::test]
async fn satisfied_guard_lets_step_run() {
    use tui_lab::scenario::model::StepExpect;

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "print('OK-READY'); import sys; sys.stdin.readline(); print('OK-GOT-INPUT'); import time; time.sleep(5)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");
    let live_hash = pool
        .with_session(Some(&id), |sess| {
            let frame = sess.observe(300).expect("baseline");
            frame.structure_hash.clone()
        })
        .await
        .expect("baseline job");

    let step = tui_lab::scenario::model::ScenarioStep {
        kind: tui_lab::scenario::model::StepKind::Act,
        params: serde_json::json!({"action": "type", "text": "go\n"}),
        expect: Some(StepExpect {
            structure_hash: Some(live_hash),
            focus_control_id: None,
            text_present: Some("OK-READY".into()),
        }),
    };
    let scenario = tui_lab::scenario::model::Scenario {
        steps: vec![step],
        ..tui_lab::scenario::model::Scenario::new("guard-ok")
    };

    let (report, delivered) = pool
        .with_session(Some(&id), move |sess| {
            let report = ScenarioRunner::run(&scenario, sess);
            // The input really landed this time.
            let out = sess
                .wait(
                    tui_lab::backend::WaitCond::Text("OK-GOT-INPUT".into()),
                    5000,
                )
                .expect("wait text");
            (report, out.met)
        })
        .await
        .expect("run job");
    assert_eq!(
        report.steps_passed, 1,
        "satisfied guard passes: {:?}",
        report.step_results
    );
    assert!(delivered, "input delivered when the guard held");
    pool.stop(&id).await.ok();
}

/// Audit finding 21: scenario replay inside a run context lands each
/// executed act in the canonical transaction ledger — a regression "which
/// replay step generated the evidence?" is answerable from the run record,
/// not a coarse "scenario ran" event.
#[tokio::test]
async fn run_in_run_enters_transaction_ledger() {
    use std::sync::Arc;
    let scenario = Scenario::new("ledger-replay")
        .act(serde_json::json!({"action": "type", "text": "echo ledger"}))
        .wait(serde_json::json!({"condition": "screen_stable", "budget_ms": 2000}));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "import sys; print('ready'); sys.stdin.read(1)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let run = Arc::new(std::sync::Mutex::new(
        tui_lab::run::RunContext::ephemeral(),
    ));
    let (report, ledger_len) = {
        let run = run.clone();
        pool.with_session(Some(&id), move |sess| {
            let run_ref: Option<&Arc<std::sync::Mutex<tui_lab::run::RunContext>>> = Some(&run);
            let rep = ScenarioRunner::run_in_run(&scenario, sess, &[], run_ref);
            let n = run.lock().unwrap().transactions().len();
            (rep, n)
        })
        .await
        .expect("run job")
    };
    assert_eq!(report.steps_failed, 0, "{:?}", report.step_results);
    assert!(
        ledger_len >= 1,
        "the act step must have produced a ledger transaction, got {ledger_len}"
    );
}

/// Audit finding 20: a recorded event wait (condition=event) replays
/// through the SAME primitive as the live tui_wait — an event-history wait
/// is a session-queue concern, not a backend read condition, and a
/// recorded one must execute identically.
#[tokio::test]
async fn event_wait_step_replays_through_shared_primitive() {
    let scenario = Scenario::new("event-wait-replay")
        // Wait for ANY event at all — met as soon as the session queue moves.
        .wait(serde_json::json!({
            "condition": "event",
            "event": { "kinds": [] },
            "budget_ms": 3000,
        }));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "import time; print('booted'); time.sleep(5)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let report = pool
        .with_session(Some(&id), move |sess| ScenarioRunner::run(&scenario, sess))
        .await
        .expect("run job");

    assert_eq!(
        report.steps_failed, 0,
        "event wait step should replay: {:?}",
        report.step_results
    );
    assert!(
        report.step_results[0].detail.contains("event wait met="),
        "detail: {}",
        report.step_results[0].detail
    );
}
