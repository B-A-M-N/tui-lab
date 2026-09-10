//! Core beta-readiness invariants from the 2026-09 production audit.
//!
//! These tests deliberately cover causal correctness and fail-closed public
//! surfaces rather than just request parsing.

use tui_lab::session::SessionPool;

#[tokio::test]
async fn guard_refuses_async_replacement_of_observed_button() {
    let pool = SessionPool::new();
    let code = r#"
import sys,time
sys.stdout.write('[ Save ]'); sys.stdout.flush()
time.sleep(1.0)
sys.stdout.write('\r[ Delete ]'); sys.stdout.flush()
data=sys.stdin.read(1)
print('\nRECEIVED',repr(data),flush=True)
"#;
    let id = pool
        .start(
            "python3",
            &["-c".to_string(), code.to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("start");
    pool.with_session(Some(&id), move |sess| {
        // start_with_spec's initial pump can race Python's first write;
        // wait until the old content is committed before arming the guard.
        let started = std::time::Instant::now();
        let old_hash = loop {
            let old = sess.observe(20).expect("observe stale screen");
            if old.viewport_text.iter().any(|r| r.contains("[ Save ]")) {
                break old.structure_hash.clone();
            }
            assert!(
                started.elapsed() < std::time::Duration::from_millis(900),
                "first frame never contained [ Save ]"
            );
        };
        let guard = tui_lab::execution::MutationGuard {
            generation: Some(sess.generation),
            structure_hash: Some(old_hash),
            focus_control_id: None,
            native_revision: None,
        };
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let err = tui_lab::execution::execute_act_with_guard(
            sess,
            &tui_lab::execution::CanonicalAction::Key {
                key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
            },
            20,
            100,
            false,
            tui_lab::execution::InputVisibility::Normal,
            tui_lab::capture::CompletionPolicy::MayBeSilent,
            Some(&guard),
        )
        .expect_err("fresh guard must see replacement");
        assert!(err.to_string().contains("stale_state"), "{}", err);
        let after = sess.observe(20).unwrap();
        assert!(!after.viewport_text.iter().any(|r| r.contains("RECEIVED")));
    })
    .await
    .expect("session job");
}

#[tokio::test]
async fn no_wait_honors_explicit_time_ceiling() {
    let pool = SessionPool::new();
    let id = pool
        .start(
            "python3",
            &["-c".to_string(), "import time; time.sleep(10)".into()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("start");
    let elapsed = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(20).unwrap();
            let t0 = std::time::Instant::now();
            // The wire surface now carries the same explicit budget; this
            // call mirrors the MCP handler's no-wait contract.
            let tx = tui_lab::execution::execute_act_with_completion(
                sess,
                &tui_lab::execution::CanonicalAction::Key {
                    key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('n')),
                },
                1000,
                0,
                true,
                tui_lab::execution::InputVisibility::Normal,
                tui_lab::capture::CompletionPolicy::NoWait,
            )
            .unwrap();
            assert_eq!(tx.settle, tui_lab::execution::SettleStatus::Skipped);
            t0.elapsed()
        })
        .await
        .unwrap();
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "{elapsed:?}"
    );
}

#[test]
fn anchored_bell_uses_supplied_counter_exactly() {
    use tui_lab::backend::wait::{WaitBaselines, WaitEvaluator};
    use tui_lab::backend::{WaitCond, WaitReason};
    // Historical bells brought the counter to 5; the caller anchored 5.
    let mut tick = wait_tick(5);
    let baselines = WaitBaselines {
        bell_seq: 5,
        interaction_seq: 0,
        fingerprint: String::new(),
    };
    let cond = WaitCond::Bell {
        after_bell_seq: Some(5),
    };
    let (met, reason) = WaitEvaluator::evaluate(&cond, &tick, &baselines, "");
    assert!(!met, "current=anchor is not a new bell");
    assert!(matches!(reason, WaitReason::Bell));
    tick.bell_seq = 6;
    let (met, _) = WaitEvaluator::evaluate(&cond, &tick, &baselines, "");
    assert!(met, "strictly newer bell resolves");
}

fn wait_tick(bell_seq: u64) -> tui_lab::backend::wait::WaitTick<'static> {
    // WaitTick borrows a screen; this pure-policy test uses one empty
    // screen for the process lifetime (the evaluator reads no mutable
    // state).
    let screen: &'static tui_lab::screen::ScreenState =
        Box::leak(Box::new(tui_lab::screen::ScreenState::new(1, 1)));
    tui_lab::backend::wait::WaitTick {
        screen,
        screen_seq: 0,
        output_seq: 0,
        bell_seq,
        interaction_seq: 0,
        screen_quiet: false,
        output_quiet: false,
        command_seq: 0,
        command_running: false,
    }
}

#[test]
fn capability_defaults_and_wait_matrix_fail_closed() {
    let caps = tui_lab::backend::Capabilities::default();
    assert!(!caps.colors);
    assert!(!caps.cell_attributes);
    assert!(!caps.signals);
    assert!(!caps.native_semantic);
    assert_eq!(
        caps.process_ownership,
        tui_lab::backend::ProcessOwnership::Unknown
    );
    for cond in [
        tui_lab::backend::WaitCond::Text("x".into()),
        tui_lab::backend::WaitCond::TextAbsent("x".into()),
        tui_lab::backend::WaitCond::ScreenChange,
        tui_lab::backend::WaitCond::ScreenStable {
            quiet_for: std::time::Duration::ZERO,
            after_screen_seq: None,
        },
        tui_lab::backend::WaitCond::ProcessExit,
        tui_lab::backend::WaitCond::Title("x".into()),
        tui_lab::backend::WaitCond::Bell {
            after_bell_seq: None,
        },
        tui_lab::backend::WaitCond::AnyActivity {
            after_interaction_seq: None,
        },
        tui_lab::backend::WaitCond::Idle {
            quiet_for: std::time::Duration::ZERO,
            after_output_seq: None,
        },
        tui_lab::backend::WaitCond::CommandDone {
            after_command_seq: None,
        },
    ] {
        assert!(
            caps.require_wait(&cond).is_err(),
            "{cond:?} must require an opt-in"
        );
    }
}

#[tokio::test]
async fn scenario_save_rejects_empty_and_accepts_intent() {
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::json;
    use tui_lab::mcp::params::TuiScenarioParams;
    use tui_lab::mcp::tools::TuiLabServer;

    fn parameters<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
        serde_json::from_value(v).unwrap()
    }

    let server = TuiLabServer::new();
    let empty: TuiScenarioParams =
        serde_json::from_value(json!({"action":"save","name":"empty"})).unwrap();
    let result = server
        .tui_scenario(parameters(serde_json::to_value(empty).unwrap()))
        .await;
    let text = serde_json::to_string(&result).unwrap_or_default();
    assert!(text.contains("at least one step"), "{text}");

    let intent: TuiScenarioParams = serde_json::from_value(json!({
        "action":"save","name":"intent-flow",
        "steps":[{"kind":"intent","params":{"target":"focused","verb":"activate"}}]
    }))
    .unwrap();
    let result = server
        .tui_scenario(parameters(serde_json::to_value(intent).unwrap()))
        .await;
    let text = serde_json::to_string(&result).unwrap_or_default();
    assert!(!text.contains("unknown step kind"), "{text}");
}
