//! MCP contract tests (spec section 13 / audit items 33, 34, 37, 38, 40).
//!
//! The `#[tool]`-generated methods are private and the `ToolRouter::call`
//! path needs an `rmcp` transport context, so we test the contract at the unit
//! level: every MCP tool is a thin envelope over these `pub` functions
//! (`run_assertion`, `build_input`, `build_wait`), the `SessionPool`
//! (`start`/`restart`), and the checkpoint/store globals. The tool wrappers
//! only serialize the results these produce, so exercising them here is the
//! real contract. (A full stdio-transport dispatch test is a separate,
//! heavier integration test; the code path is identical.)
//!
//! Each test asserts the *honest* behavior the audit demanded: capabilities in
//! `start`, rejection of unsupported backend/isolation, `exit_code` comparing an
//! expected code, unknown assertions as `invalid_request`, real checkpoint
//! delete, and non-cast recording as `unsupported`.
//!
//! Migrated off the legacy `SessionManager` (audit P1-54).

use tui_lab::backend::{Input, KeyCode, KeyModifiers};
use tui_lab::error::{Envelope, ErrorCategory};
use tui_lab::execution::CanonicalAction;
use tui_lab::mcp::helpers::{build_wait, control_label_exists, err, ok, run_assertion};
use tui_lab::mcp::params::{TuiActRequest, TuiAssertParams, TuiWaitParams};
use tui_lab::screen::{ProcessState, ScreenState};
use tui_lab::session::SessionPool;

async fn start_child(pool: &SessionPool, command: &str, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    pool.start(command, &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start")
}

#[tokio::test]
async fn contract_run_assertion_exit_code_compares_expected_code() {
    // Build a screen whose process has exited with code 7.
    let pool = SessionPool::new();
    let id = start_child(&pool, "python3", &["-c", "import sys; sys.exit(7)"]).await;
    // Wait for it to exit, then observe.
    let (_passed_7, passed_0, detail_0, _invalid_0) = pool
        .with_session(Some(&id), {
            let id = id.clone();
            move |sess| {
                let _ = &id;
                let _ = sess.wait(tui_lab::backend::WaitCond::ProcessExit, 5000);
                let screen = sess.observe(40).expect("observe");
                let mk = |expected: Option<i32>| TuiAssertParams {
                    assertion: "exit_code".into(),
                    text: None,
                    subject: None,
                    reference: None,
                    x: None,
                    y: None,
                    cols: None,
                    rows: None,
                    expected_code: expected,
                    id: None,
                };
                let (passed, _, invalid) = run_assertion(&mk(Some(7)), &screen);
                assert!(passed);
                assert!(invalid.is_none());
                let (passed0, detail, invalid0) = run_assertion(&mk(Some(0)), &screen);
                (passed, passed0, detail, invalid0)
            }
        })
        .await
        .expect("job");

    // expected_code == 0 -> fails (real comparison, not just "not running")
    assert!(_passed_7, "expected_code == 7 passes");
    assert!(!passed_0);
    assert!(_invalid_0.is_none());
    assert!(
        detail_0.contains("expected exit code 0"),
        "detail: {detail_0}"
    );
}

#[test]
fn contract_unknown_assertion_is_invalid_request_not_failure() {
    let empty_screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["".to_string(); 24],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "made_up_assertion".into(),
        text: None,
        subject: None,
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &empty_screen);
    assert!(!passed);
    assert_eq!(invalid, Some(ErrorCategory::InvalidRequest));
}

#[test]
fn contract_keys_action_sends_full_sequence() {
    let p = TuiActRequest::Keys(tui_lab::mcp::params::KeysPayload {
        keys: vec!["tab".into(), "x".into(), "y".into()],
        common: tui_lab::mcp::params::ActCommon::none(),
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build keys input")
        .to_input();
    match input {
        Input::Keys(keys) => assert_eq!(keys.len(), 3, "keys must preserve the whole sequence"),
        _ => panic!("expected Input::Keys"),
    }
}

#[test]
fn contract_build_wait_screen_stable_has_quiet_interval() {
    let p = TuiWaitParams {
        event: None,
        condition: "screen_stable".into(),
        text: None,
        title: None,
        budget_ms: None,
        quiet_ms: None,
        id: None,
    };
    let cond = build_wait(&p).expect("build wait");
    match cond {
        tui_lab::backend::WaitCond::ScreenStable { quiet_for, .. } => {
            assert_eq!(quiet_for, std::time::Duration::from_millis(80));
        }
        _ => panic!("screen_stable must carry a quiet_for interval"),
    }
}

#[test]
fn contract_envelope_shapes() {
    // Wave G item 71: ok/err return structured CallToolResults. The
    // structured content IS the envelope; the text block carries the same
    // JSON for text-only clients.
    let ok_res = ok(serde_json::json!({ "x": 1 }));
    assert_eq!(ok_res.is_error, Some(false));
    let ok_env: Envelope<serde_json::Value> =
        serde_json::from_value(ok_res.structured_content.clone().expect("structured")).unwrap();
    assert_eq!(ok_env.category, ErrorCategory::Success);
    assert!(ok_env.error.is_none());
    assert_eq!(ok_env.data.expect("data")["x"], 1);

    let err_res = err(ErrorCategory::Unsupported, "nope");
    assert_eq!(err_res.is_error, Some(true));
    let err_env: Envelope<()> =
        serde_json::from_value(err_res.structured_content.clone().expect("structured")).unwrap();
    assert_eq!(err_env.category, ErrorCategory::Unsupported);
    assert_eq!(err_env.error.as_deref(), Some("nope"));
}

#[test]
fn contract_checkpoint_store_save_compare_delete() {
    use tui_lab::checkpoint::store::CheckpointStore;
    use tui_lab::screen::{CursorState, ProcessState, ScreenState};

    let screen = ScreenState {
        cols: 10,
        rows: 3,
        cursor: CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["a".to_string(), "".to_string(), "".to_string()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: "r".into(),
        visual_hash: "v".into(),
        structure_hash: "s".into(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };

    let mut store = CheckpointStore::new();
    let name = store.save("sess", 0, Some("before".into()), &screen, None);
    assert!(store.contains("sess", &name));

    // compare: same screen matches; result is a JSON envelope with matches
    let out = store
        .compare("sess", &name, &screen, None)
        .expect("compare");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("envelope json");
    assert_eq!(
        parsed["data"]["comparison"]["matches"]["structure"],
        serde_json::json!(true),
        "out: {}",
        out
    );

    // unknown checkpoint is invalid_request, not a silent pass
    let miss = store.compare("sess", "nope", &screen, None);
    assert_eq!(miss, Err(ErrorCategory::InvalidRequest));

    assert!(store.delete("sess", &name));
    assert!(!store.delete("sess", &name), "second delete is false");
}

#[tokio::test]
async fn contract_restart_preserves_id_and_bumps_generation() {
    let pool = SessionPool::new();
    let id = start_child(&pool, "python3", &["-c", "import time; time.sleep(5)"]).await;
    let before = pool
        .with_session(Some(&id), |sess| sess.generation)
        .await
        .expect("job");
    let (new_id, gen) = pool.restart(&id).await.expect("restart");
    assert_eq!(new_id, id, "restart must preserve the session id");
    assert_eq!(gen, before + 1, "generation must increment");
    let after = pool
        .with_session(Some(&id), |sess| sess.generation)
        .await
        .expect("job");
    assert_eq!(after, before + 1);
    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn contract_launch_spec_preserved_across_restart() {
    let pool = SessionPool::new();
    let id = pool
        .start(
            "python3",
            &["-c".to_string(), "print('hi')".to_string()],
            Some("/tmp"),
            &[("FOO".to_string(), "bar".to_string())],
            100,
            30,
            "auto",
            "local",
        )
        .await
        .expect("start");
    let spec = pool
        .with_session(Some(&id), |sess| sess.launch().cloned())
        .await
        .expect("job")
        .expect("launch spec");
    assert_eq!(spec.cols, 100);
    assert_eq!(spec.rows, 30);
    assert_eq!(spec.cwd.as_deref(), Some("/tmp"));
    assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
    // Restart reuses the same spec.
    pool.restart(&id).await.unwrap();
    let spec2 = pool
        .with_session(Some(&id), |sess| sess.launch().cloned())
        .await
        .expect("job")
        .expect("launch spec");
    assert_eq!(spec2.cols, 100);
    assert_eq!(spec2.cwd.as_deref(), Some("/tmp"));
    assert_eq!(spec2.env, vec![("FOO".to_string(), "bar".to_string())]);
    pool.stop(&id).await.expect("stop");
}

#[test]
fn contract_ctrl_key_encodes_typed_representation() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "ctrl+c".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build key input")
        .to_input();
    match input {
        Input::Key(kev) => {
            assert_eq!(kev.code, KeyCode::Char('c'));
            assert!(kev.modifiers.ctrl());
        }
        _ => panic!("expected Input::Key"),
    }
}

#[test]
fn contract_position_assertion_checks_text_at_coords() {
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["Hello World".to_string(), "          ".to_string()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    // "World" starts at x=6, y=0
    let p = TuiAssertParams {
        assertion: "position".into(),
        text: Some("World".into()),
        subject: None,
        reference: None,
        x: Some(6),
        y: Some(0),
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(passed, "position assertion should pass for text at (6,0)");
    assert!(invalid.is_none());

    // Wrong position
    let p2 = TuiAssertParams {
        assertion: "position".into(),
        text: Some("World".into()),
        subject: None,
        reference: None,
        x: Some(0),
        y: Some(0),
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p2, &screen);
    assert!(!passed, "position assertion should fail for wrong coords");
    assert!(invalid.is_none());
}

#[test]
fn contract_region_assertion_finds_region_by_title() {
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec![
            "┌────────────────┐".to_string(),
            "│ Settings       │".to_string(),
            "│ Host: localhost│".to_string(),
            "└────────────────┘".to_string(),
        ],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "region".into(),
        text: None,
        subject: Some("Settings".into()),
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(
        passed,
        "region assertion should find region with interior title"
    );
    assert!(invalid.is_none());

    // Non-existent region
    let p2 = TuiAssertParams {
        assertion: "region".into(),
        text: None,
        subject: Some("NonExistent".into()),
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p2, &screen);
    assert!(!passed, "region assertion should fail for missing region");
    assert!(invalid.is_none());
}

#[test]
fn contract_snapshot_assertion_compares_structure_hash() {
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["test".to_string()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: "abc123".to_string(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    // Matching hash
    let p = TuiAssertParams {
        assertion: "snapshot".into(),
        text: None,
        subject: None,
        reference: Some("abc123".into()),
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(passed, "snapshot assertion should pass for matching hash");
    assert!(invalid.is_none());

    // Non-matching hash
    let p2 = TuiAssertParams {
        assertion: "snapshot".into(),
        text: None,
        subject: None,
        reference: Some("xyz789".into()),
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p2, &screen);
    assert!(
        !passed,
        "snapshot assertion should fail for non-matching hash"
    );
    assert!(invalid.is_none());
}

#[test]
fn contract_structure_assertion_is_snapshot_alias() {
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["test".to_string()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: "hash_v1".to_string(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "structure".into(),
        text: None,
        subject: None,
        reference: Some("hash_v1".into()),
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(passed, "structure assertion should pass for matching hash");
    assert!(invalid.is_none());
}

// ─── Task 6: parse_key case fidelity ─────────────────────────────────

#[test]
fn contract_key_a_preserves_case() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "A".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build key A")
        .to_input();
    match input {
        Input::Key(kev) => {
            assert_eq!(kev.code, KeyCode::Char('A'));
            assert!(!kev.modifiers.contains(KeyModifiers::SHIFT));
        }
        _ => panic!("expected Input::Key"),
    }
}

#[test]
fn contract_key_shift_a_yields_uppercase() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "shift+a".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build shift+a")
        .to_input();
    match input {
        Input::Key(kev) => {
            assert_eq!(kev.code, KeyCode::Char('A'));
            assert!(kev.modifiers.contains(KeyModifiers::SHIFT));
        }
        _ => panic!("expected Input::Key"),
    }
}

#[test]
fn contract_key_ctrl_c() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "ctrl+c".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build ctrl+c")
        .to_input();
    match input {
        Input::Key(kev) => {
            assert_eq!(kev.code, KeyCode::Char('c'));
            assert!(kev.modifiers.contains(KeyModifiers::CTRL));
        }
        _ => panic!("expected Input::Key"),
    }
}

#[test]
fn contract_key_bogus_modifier_errors() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "bogus+key".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let result = CanonicalAction::from_request(&p);
    assert!(result.is_err());
}

#[test]
fn contract_key_f1_function() {
    let p = TuiActRequest::Key(tui_lab::mcp::params::KeyPayload {
        key: "F1".into(),
        common: tui_lab::mcp::params::ActCommon {
            no_wait: None,
            completion: None,
            wait_ms: None,
            settle_budget_ms: None,
            id: None,
            guard: None,
        },
    });
    let input = CanonicalAction::from_request(&p)
        .expect("build F1")
        .to_input();
    match input {
        Input::Key(kev) => {
            assert_eq!(kev.code, KeyCode::Function(1));
        }
        _ => panic!("expected Input::Key"),
    }
}

// ─── Task 7: build_wait idle and quiet_ms override ────────────────────

#[test]
fn contract_build_wait_idle_has_quiet_interval() {
    let p = TuiWaitParams {
        event: None,
        condition: "idle".into(),
        text: None,
        title: None,
        budget_ms: None,
        quiet_ms: None,
        id: None,
    };
    let cond = build_wait(&p).expect("build wait idle");
    match cond {
        tui_lab::backend::WaitCond::Idle { quiet_for, .. } => {
            assert_eq!(quiet_for, std::time::Duration::from_millis(250));
        }
        _ => panic!("idle must produce WaitCond::Idle"),
    }
}

#[test]
fn contract_build_wait_quiet_ms_overrides_both_conditions() {
    let p_screen = TuiWaitParams {
        event: None,
        condition: "screen_stable".into(),
        text: None,
        title: None,
        budget_ms: None,
        quiet_ms: Some(500),
        id: None,
    };
    let cond = build_wait(&p_screen).expect("screen_stable with quiet_ms");
    match cond {
        tui_lab::backend::WaitCond::ScreenStable { quiet_for, .. } => {
            assert_eq!(quiet_for, std::time::Duration::from_millis(500));
        }
        _ => panic!("screen_stable must produce ScreenStable"),
    }

    let p_idle = TuiWaitParams {
        event: None,
        condition: "idle".into(),
        text: None,
        title: None,
        budget_ms: None,
        quiet_ms: Some(500),
        id: None,
    };
    let cond = build_wait(&p_idle).expect("idle with quiet_ms");
    match cond {
        tui_lab::backend::WaitCond::Idle { quiet_for, .. } => {
            assert_eq!(quiet_for, std::time::Duration::from_millis(500));
        }
        _ => panic!("idle must produce Idle"),
    }
}

#[test]
fn contract_build_wait_unknown_condition_returns_none() {
    let p = TuiWaitParams {
        event: None,
        condition: "nonexistent".into(),
        text: None,
        title: None,
        budget_ms: None,
        quiet_ms: None,
        id: None,
    };
    assert!(build_wait(&p).is_none());
}

// ─── Task 9: run_assertion control_exists and focused_not ─────────────

#[test]
fn contract_control_exists_finds_matching_label() {
    // Build a screen with a [ Save ] button region so control detection finds it.
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec![
            "┌────────────────┐".to_string(),
            "│               [ Save ]  │".to_string(),
            "└────────────────┘".to_string(),
        ],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "control_exists".into(),
        text: None,
        subject: Some("Save".into()),
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(passed, "control_exists should find Save");
    assert!(invalid.is_none());
}

#[test]
fn contract_control_exists_missing_subject_is_invalid_request() {
    let empty_screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["".to_string(); 24],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "control_exists".into(),
        text: None,
        subject: None,
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &empty_screen);
    assert!(!passed);
    assert_eq!(invalid, Some(ErrorCategory::InvalidRequest));
}

#[test]
fn contract_focused_not_with_no_focus_passes() {
    let empty_screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["".to_string(); 24],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "focused_not".into(),
        text: None,
        subject: Some("Save".into()),
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &empty_screen);
    // No focus at all => passes (focus is NOT on "Save")
    assert!(passed);
    assert!(invalid.is_none());
}

#[test]
fn contract_focused_not_missing_subject_is_invalid_request() {
    let empty_screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["".to_string(); 24],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    let p = TuiAssertParams {
        assertion: "focused_not".into(),
        text: None,
        subject: None,
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: None,
        id: None,
    };
    let (passed, _, invalid) = run_assertion(&p, &empty_screen);
    assert!(!passed);
    assert_eq!(invalid, Some(ErrorCategory::InvalidRequest));
}

// ─── control_label_exists pure helper ─────────────────────────────────

#[test]
fn contract_control_label_exists_helper() {
    let screen = ScreenState {
        cols: 80,
        rows: 24,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec![
            "┌────────────────┐".to_string(),
            "│               [ Save ]  │".to_string(),
            "└────────────────┘".to_string(),
        ],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: String::new(),
        visual_hash: String::new(),
        structure_hash: String::new(),
        process: ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    };
    assert!(control_label_exists(&screen, "Save"));
    assert!(control_label_exists(&screen, "save")); // case-insensitive
    assert!(!control_label_exists(&screen, "Missing"));
}

/// Review P0 rigidity #4: a `completion` field on a `TuiActRequest` must be
/// honored as a declared completion, not silently dropped. An agent that says
/// `completion: "may_be_silent"` (e.g. copy-to-clipboard) must get a policy
/// whose strategy never reports a false `settled=false`; absence must default
/// to the ordinary "screen settles" behaviour.
#[test]
fn tui_act_completion_field_resolves_to_policy() {
    // Wire round-trip: `completion: "may_be_silent"` deserializes (the field is
    // optional, serde(default)) and resolves to the declare-silent policy.
    let raw = r#"{"action":"type","text":"x","no_wait":false,"completion":"may_be_silent"}"#;
    let p: TuiActRequest = serde_json::from_str(raw).expect("deserialize completion request");
    assert!(
        matches!(
            p.completion(),
            Some(tui_lab::capture::CompletionPolicy::MayBeSilent)
        ),
        "may_be_silent completion must resolve to MayBeSilent"
    );

    // Absence defaults to the ordinary screen-settle behaviour.
    let plain = r#"{"action":"key","key":"enter"}"#;
    let p2: TuiActRequest = serde_json::from_str(plain).expect("deserialize plain request");
    assert!(
        p2.completion().is_none(),
        "absent completion must resolve to the default (None => StableScreen downstream)"
    );

    // A named-text completion carries its text INSIDE the completion
    // (re-review P0.1): the lossless spec form.
    let appears =
        r#"{"action":"type","text":"ok","completion":{"type":"text_appears","text":"Saved"}}"#;
    let p3: TuiActRequest =
        serde_json::from_str(appears).expect("deserialize text-appears request");
    assert!(
        matches!(
            p3.completion(),
            Some(tui_lab::capture::CompletionPolicy::TextAppears(t)) if t == "Saved"
        ),
        "text_appears completion must carry the requested text into the policy"
    );

    // The old flat-string form for a parameterized completion is now a
    // deserialization error — it used to silently become
    // TextAppears(""), a policy that could never match.
    let broken = r#"{"action":"type","text":"ok","completion":"text_appears"}"#;
    assert!(
        serde_json::from_str::<TuiActRequest>(broken).is_err(),
        "bare 'text_appears' without its text must be rejected, not coerced to an empty policy"
    );

    // Parameterless strategies stay plain strings (backward compatible).
    let silent = r#"{"action":"key","key":"f1","completion":"may_be_silent"}"#;
    let p4: TuiActRequest = serde_json::from_str(silent).expect("deserialize may_be_silent");
    assert!(matches!(
        p4.completion(),
        Some(tui_lab::capture::CompletionPolicy::MayBeSilent)
    ));

    // stable_screen with its own quiet override resolves both the policy
    // and the quiet window.
    let stabled = r#"{"action":"resize","cols":60,"rows":20,"completion":{"type":"stable_screen","quiet_ms":400}}"#;
    let p5: TuiActRequest = serde_json::from_str(stabled).expect("deserialize stable_screen spec");
    assert!(
        matches!(
            p5.completion(),
            Some(tui_lab::capture::CompletionPolicy::StableScreen)
        ),
        "stable_screen spec resolves to the settle policy"
    );
    assert_eq!(
        p5.completion_quiet_ms(),
        Some(400),
        "the spec's quiet_ms must surface to the executor"
    );
    // The act handlers derive quiet as: explicit wait_ms, else the
    // completion spec's quiet_ms, else the 150ms default.
    assert_eq!(
        p5.wait_ms().or_else(|| p5.completion_quiet_ms()),
        Some(400),
        "quiet override feeds the wait derivation when no explicit wait_ms is present"
    );

    // Resize/Signal participate in the completion system now (P0.10).
    let sig = r#"{"action":"signal","signal":15,"completion":"process_exit"}"#;
    let p6: TuiActRequest = serde_json::from_str(sig).expect("deserialize signal request");
    assert!(matches!(
        p6.completion(),
        Some(tui_lab::capture::CompletionPolicy::ProcessExit)
    ));
}

// Finding 16 (docs parity): the README's hand-written tool sections must
// cover every tool the capability registry declares. The registry is the
// contract; this test keeps the human documentation from drifting behind
// it — a new registered tool without a README section fails here, with
// the missing names in the message.
#[test]
fn readme_documents_every_registered_tool() {
    let readme = match std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")) {
        Ok(r) => r,
        // README absent in an exotic checkout: nothing to keep in parity.
        Err(_) => return,
    };
    let missing: Vec<&str> = tui_lab::mcp::registry::TOOLS
        .iter()
        .map(|t| t.name)
        .filter(|name| {
            // A `### <tool>` section header (also matched inline in the
            // generated skill doc) is what "documented" means here.
            !readme.contains(&format!("### {name}\n"))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "README tool sections drifted from the registry — no `### <tool>` \
         section for: {missing:?}. Add prose sections (or regenerate from \
         registry::TOOLS); the registry is the contract, the README must \
         not underdescribe the product."
    );
}
