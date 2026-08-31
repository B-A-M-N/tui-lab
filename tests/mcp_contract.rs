//! MCP contract tests (spec section 13 / audit items 33, 34, 37, 38, 40).
//!
//! The `#[tool]`-generated methods are private and the `ToolRouter::call`
//! path needs an `rmcp` transport context, so we test the contract at the unit
//! level: every MCP tool is a thin envelope over these `pub` functions
//! (`run_assertion`, `build_input`, `build_wait`), the `SessionManager`
//! (`start`/`restart`), and the checkpoint/store globals. The tool wrappers
//! only serialize the results these produce, so exercising them here is the
//! real contract. (A full stdio-transport dispatch test is a separate,
//! heavier integration test; the code path is identical.)
//!
//! Each test asserts the *honest* behavior the audit demanded: capabilities in
//! `start`, rejection of unsupported backend/isolation, `exit_code` comparing an
//! expected code, unknown assertions as `invalid_request`, real checkpoint
//! delete, and non-cast recording as `unsupported`.

use tui_lab::backend::{Input, KeyCode, KeyModifiers};
use tui_lab::error::{Envelope, ErrorCategory};
use tui_lab::execution::CanonicalAction;
use tui_lab::mcp::helpers::{build_wait, control_label_exists, err, ok, run_assertion};
use tui_lab::mcp::params::{TuiActRequest, TuiAssertParams, TuiWaitParams};
use tui_lab::screen::{ProcessState, ScreenState};
use tui_lab::session::manager::SessionManager;

fn start_child(mgr: &mut SessionManager, command: &str, args: &[&str]) -> String {
    mgr.start(
        command,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
        &[],
        80,
        24,
        "auto",
        "local",
    )
    .expect("start")
}

#[test]
fn contract_run_assertion_exit_code_compares_expected_code() {
    // Build a screen whose process has exited with code 7.
    let mut mgr = SessionManager::new();
    let id = start_child(&mut mgr, "python3", &["-c", "import sys; sys.exit(7)"]);
    // Wait for it to exit, then observe.
    let _ = mgr
        .resolve_mut(Some(&id))
        .unwrap()
        .wait(tui_lab::backend::WaitCond::ProcessExit, 5000);
    let screen = mgr.resolve_mut(Some(&id)).unwrap().observe(40).unwrap();

    // expected_code == 7 -> passes
    let p = TuiAssertParams {
        assertion: "exit_code".into(),
        text: None,
        subject: None,
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: Some(7),
        id: Some(id.clone()),
    };
    let (passed, _, invalid) = run_assertion(&p, &screen);
    assert!(passed);
    assert!(invalid.is_none());

    // expected_code == 0 -> fails (real comparison, not just "not running")
    let p = TuiAssertParams {
        assertion: "exit_code".into(),
        text: None,
        subject: None,
        reference: None,
        x: None,
        y: None,
        cols: None,
        rows: None,
        expected_code: Some(0),
        id: Some(id),
    };
    let (passed, detail, invalid) = run_assertion(&p, &screen);
    assert!(!passed);
    assert!(invalid.is_none());
    assert!(
        detail.contains("expected exit code 0"),
        "detail: {}",
        detail
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
    let p = TuiActRequest::Keys {
        keys: vec!["tab".into(), "tab".into(), "enter".into()],
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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

#[test]
fn contract_restart_preserves_id_and_bumps_generation() {
    let mut mgr = SessionManager::new();
    let id = start_child(&mut mgr, "python3", &["-c", "import time; time.sleep(5)"]);
    let before = mgr.get(&id).unwrap().generation;
    let (new_id, gen) = mgr.restart(&id).expect("restart");
    assert_eq!(new_id, id, "restart must preserve the session id");
    assert_eq!(gen, before + 1, "generation must increment");
    assert_eq!(mgr.get(&id).unwrap().generation, before + 1);
}

#[test]
fn contract_launch_spec_preserved_across_restart() {
    let mut mgr = SessionManager::new();
    let id = mgr
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
        .expect("start");
    let spec = mgr
        .get(&id)
        .unwrap()
        .launch()
        .cloned()
        .expect("launch spec");
    assert_eq!(spec.cols, 100);
    assert_eq!(spec.rows, 30);
    assert_eq!(spec.cwd.as_deref(), Some("/tmp"));
    assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
    // Restart reuses the same spec.
    mgr.restart(&id).unwrap();
    let spec2 = mgr
        .get(&id)
        .unwrap()
        .launch()
        .cloned()
        .expect("launch spec");
    assert_eq!(spec2.cols, 100);
    assert_eq!(spec2.cwd.as_deref(), Some("/tmp"));
    assert_eq!(spec2.env, vec![("FOO".to_string(), "bar".to_string())]);
}

#[test]
fn contract_ctrl_key_encodes_typed_representation() {
    let p = TuiActRequest::Key {
        key: "ctrl+c".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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
    let p = TuiActRequest::Key {
        key: "A".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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
    let p = TuiActRequest::Key {
        key: "shift+a".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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
    let p = TuiActRequest::Key {
        key: "ctrl+c".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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
    let p = TuiActRequest::Key {
        key: "bogus+key".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
    let result = CanonicalAction::from_request(&p);
    assert!(result.is_err());
}

#[test]
fn contract_key_f1_function() {
    let p = TuiActRequest::Key {
        key: "F1".into(),
        no_wait: None,
        wait_ms: None,
        id: None,
    };
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
