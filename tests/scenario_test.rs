// Tests for the scenario recorder/runner/replay system (spec item 39).

use tui_lab::scenario::{Scenario, ScenarioMetadata, ScenarioRecorder, ScenarioRunner, StepKind};

#[test]
fn scenario_model_new() {
    let scenario = Scenario::new("test-scenario");
    assert_eq!(scenario.name, "test-scenario");
    assert_eq!(scenario.schema, "tui-lab/scenario/v1");
    assert!(scenario.steps.is_empty());
    assert!(scenario.is_valid() == false);
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

#[test]
fn scenario_runner_executes_steps() {
    let scenario = Scenario::new("runner-test")
        .act(serde_json::json!({"action": "key", "key": "enter"}))
        .assert(serde_json::json!({"assertion": "text", "text": "test"}));

    let mut mgr = tui_lab::session::SessionManager::new();
    let args: Vec<String> = vec!["-c".into(), "print('test')".into()];
    let id = mgr
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let report = ScenarioRunner::run(&scenario, sess, |_params, _screen| {
        (true, "mock assertion passed".into())
    });

    assert_eq!(report.scenario_name, "runner-test");
    assert_eq!(report.steps_total, 2);
    assert_eq!(report.steps_passed, 2);
    assert_eq!(report.steps_failed, 0);
}

#[test]
fn scenario_runner_detects_failures() {
    let scenario = Scenario::new("fail-test")
        .act(serde_json::json!({"action": "key", "key": "enter"}))
        .assert(serde_json::json!({"assertion": "text", "text": "never_matches"}));

    let mut mgr = tui_lab::session::SessionManager::new();
    let args: Vec<String> = vec!["-c".into(), "print('test')".into()];
    let id = mgr
        .start("python3", &args, None, &[], 80, 24, "auto", "local")
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let report = ScenarioRunner::run(&scenario, sess, |_params, _screen| {
        (false, "mock assertion failed".into())
    });

    assert_eq!(report.steps_total, 2);
    assert_eq!(report.steps_passed, 1);
    assert_eq!(report.steps_failed, 1);
    assert_eq!(report.step_results[1].detail, "mock assertion failed");
}
