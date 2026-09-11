// Tests for the scenario recorder/runner/replay system (spec item 39).

use tui_lab::scenario::{
    model::ScenarioLaunch, Scenario, ScenarioMetadata, ScenarioRecorder, ScenarioRunner, StepKind,
};

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
    assert!(echoed, "typed text must reach the PTY (terminal echo)");
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
            ..Default::default()
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
    assert!(!leaked, "guarded input must NOT reach the app");
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
            ..Default::default()
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

    let run = Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
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

/// Audit finding 5, the fail-fast contract: under the default stop policy
/// a failed step HALTS the replay — later steps are skipped (each marked
/// `skipped_due_to_prior_failure`, counted separately), the report says
/// `stopped_on_failure`, and skipped steps never count as failures. The
/// core proof that the later input was NOT sent: the scenario types a
/// second marker after a failing assert, and that marker must be absent
/// from the screen.
#[tokio::test]
async fn stop_policy_halts_and_marks_skipped() {
    let scenario = Scenario::new("fail-fast")
        .act(serde_json::json!({"action": "type", "text": "first-ok"}))
        // Fails: the text never appears.
        .assert(serde_json::json!({"assertion": "text", "text": "never-appears-xyz"}))
        // Would corrupt the app under continue semantics; must NOT run.
        .act(serde_json::json!({"action": "type", "text": "second-marker-never-sent"}))
        .assert(serde_json::json!({"assertion": "text", "text": "also-never-checked"}));

    let pool = tui_lab::session::SessionPool::new();
    let args: Vec<String> = vec![
        "-c".into(),
        "print('test'); import time; time.sleep(10)".into(),
    ];
    let id = pool
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");

    let (report, screen_has_second) = pool
        .with_session(Some(&id), move |sess| {
            let rep = ScenarioRunner::run(&scenario, sess);
            let screen = sess.observe(60).expect("post-screen");
            let has_second = screen
                .viewport_text
                .iter()
                .any(|r| r.contains("second-marker-never-sent"));
            (rep, has_second)
        })
        .await
        .expect("run job");

    assert_eq!(report.steps_total, 4);
    assert_eq!(
        report.status,
        tui_lab::scenario::runner::RunStatus::StoppedOnFailure
    );
    assert_eq!(report.steps_passed, 1, "{:?}", report.step_results);
    assert_eq!(report.steps_failed, 1, "{:?}", report.step_results);
    assert_eq!(report.steps_skipped, 2, "{:?}", report.step_results);
    // Steps 2 and 3 are marked skipped with the reason naming the halt.
    for skipped in &report.step_results[2..] {
        assert!(
            skipped.detail.contains("skipped_due_to_prior_failure"),
            "{:?}",
            skipped
        );
    }
    assert!(
        !report.step_results[2].passed && !report.step_results[3].passed,
        "skipped steps are not passes"
    );
    // The input after the failure genuinely never landed.
    assert!(
        !screen_has_second,
        "stop policy must not send input after the failed step"
    );
}

/// Audit finding 5, the explicit census path: `on_failure=continue` runs
/// every step regardless, the status stays `completed`, nothing is
/// skipped — and the wire override wins over a scenario that recorded
/// stop.
#[tokio::test]
async fn continue_policy_runs_every_step() {
    let mut scenario = Scenario::new("census")
        .assert(serde_json::json!({"assertion": "text", "text": "nope-1"}))
        .assert(serde_json::json!({"assertion": "text", "text": "nope-2"}));
    // Scenario records stop; the CALLER overrides with continue.
    scenario.on_failure = tui_lab::scenario::model::FailurePolicy::Stop;

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
        .with_session(Some(&id), move |sess| {
            ScenarioRunner::run_in_run_with_policy(
                &scenario,
                sess,
                &[],
                None,
                Some(tui_lab::scenario::model::FailurePolicy::Continue),
            )
        })
        .await
        .expect("run job");

    assert_eq!(
        report.status,
        tui_lab::scenario::runner::RunStatus::Completed
    );
    assert_eq!(report.steps_failed, 2, "{:?}", report.step_results);
    assert_eq!(report.steps_skipped, 0, "{:?}", report.step_results);
    assert_eq!(report.steps_passed, 0, "{:?}", report.step_results);
}

// Audit findings 18/24: a scenario-owned target is launched exactly as
// declared, replayed, cleaned up, and relaunchable by generation for
// repeats. The runner refuses any launch-spec mismatch rather than
// borrowing an inherited target.
#[tokio::test]
async fn scenario_owned_launch_replays_and_cleans_up() {
    let scenario = Scenario {
        inherit_session: false,
        launch: Some(ScenarioLaunch {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('OWNED'); import sys,time; sys.stdin.read(1)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        }),
        ..Scenario::new("owned-replay")
    };
    let scenario = scenario
        .act(serde_json::json!({ "action": "type", "text": "A" }))
        .assert(serde_json::json!({ "assertion": "text", "text": "A" }));
    assert!(scenario.is_valid());

    let pool = tui_lab::session::SessionPool::new();
    let id = pool
        .start(
            "python3",
            &[
                "-c".into(),
                "print('OWNED'); import sys,time; sys.stdin.read(1)".into(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("owned launch");

    let run = tui_lab::run::RunContext::ephemeral();
    let run_ref = std::sync::Arc::new(std::sync::Mutex::new(run));
    let owned_id = id.clone();
    let scenario_for_run = scenario.clone();
    let report = pool
        .with_session(Some(&id), move |sess| {
            let run = run_ref.clone();
            tui_lab::scenario::runner::ScenarioRunner::run_in_run(
                &scenario_for_run,
                sess,
                &[],
                Some(&run),
            )
        })
        .await
        .expect("owned replay");
    assert!(
        report.steps_failed == 0 && report.steps_skipped == 0,
        "{report:?}"
    );

    // The runner treats a different session as a contract mismatch; this
    // is the guard that makes scenario ownership meaningful.
    let wrong_id = pool
        .start(
            "python3",
            &[
                "-c".into(),
                "print('OTHER'); import sys,time; sys.stdin.read(1)".into(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("other launch");
    let scenario_wrong = scenario.clone();
    let mismatch = pool
        .with_session(Some(&wrong_id), move |sess| {
            tui_lab::scenario::runner::ScenarioRunner::run(&scenario_wrong, sess)
        })
        .await
        .expect("mismatch replay");
    assert_eq!(mismatch.steps_skipped, mismatch.steps_total);
    assert!(
        mismatch
            .step_results
            .iter()
            .all(|r| r.detail.contains("scenario_ownership_mismatch")),
        "{mismatch:?}"
    );

    pool.stop(&owned_id).await.expect("owned cleanup");
    pool.stop(&wrong_id).await.expect("other cleanup");
}

// Audit finding 18: restart-based repeats are valid only for scenario-owned
// launches; inherited sessions cannot manufacture a reset contract.
#[tokio::test]
async fn inherited_repeat_without_reset_contract_is_refused() {
    let scenario =
        Scenario::new("repeat-inherited").act(serde_json::json!({ "action": "type", "text": "R" }));
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
        .start(
            "python3",
            &["-c".into(), "import sys,time; sys.stdin.read(1)".into()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("session");
    let aggregate = pool
        .with_session(Some(&id), move |sess| {
            tui_lab::scenario::runner::ScenarioRunner::run_repeat_with_reset(
                &scenario,
                sess,
                &[],
                None,
                None,
                2,
                tui_lab::scenario::runner::ResetMode::Continue,
            )
        })
        .await
        .expect("repeat refusal");
    assert_eq!(aggregate.passed_runs, 0);
    assert_eq!(aggregate.failed_runs, 1);
    assert_eq!(
        aggregate.verdict,
        tui_lab::scenario::runner::FlakinessVerdict::StableFailure
    );
    let first = aggregate.first_run.expect("refusal report");
    assert_eq!(first.steps_skipped, first.steps_total);
    assert!(
        first
            .step_results
            .iter()
            .all(|r| r.detail.contains("no reset contract")),
        "{first:?}"
    );
    pool.stop(&id).await.expect("cleanup");
}

#[test]
fn recording_fidelity_survives_stop_and_artifact_metadata() {
    // Session-level recording lifecycle: provenance chosen at start is the
    // same authority returned at stop and placed in the artifact summary.
    let mut s = tui_lab::session::state::Session::new("fidelity".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            "print('REC'); import time; time.sleep(5)".into(),
        ],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("start");
    s.enable_recording(false);
    let start_meta = s.recording_fidelity().expect("recorder");
    assert_eq!(start_meta["boundary"], "pty-bytes");
    assert_eq!(start_meta["lossy"], false);
    s.record_output(b"marker\n");
    let rec = s.stop_recording().expect("recording");
    let (ndjson, stop_meta) = {
        let r = rec.lock().unwrap();
        (r.to_ndjson(), r.fidelity_metadata())
    };
    let header: serde_json::Value =
        serde_json::from_str(ndjson.first().unwrap()).expect("cast header");
    assert_eq!(header["tui_lab"], start_meta);
    assert_eq!(stop_meta, start_meta);
    s.stop().ok();
}
