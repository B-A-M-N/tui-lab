// Tests for the active audit driver: keyboard, focus, resize, clipping audits.

#[test]
fn test_keyboard_audit_detects_states() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "import sys; print('tab test'); sys.stdout.flush(); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::keyboard_audit(sess, 3);

    assert!(
        !findings.is_empty(),
        "keyboard audit should produce findings"
    );
    assert_eq!(findings[0].category, "keyboard");
}

#[test]
fn test_focus_audit_detects_focus() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "import sys; print('focus test'); sys.stdout.flush(); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::focus_audit(sess);

    assert!(!findings.is_empty(), "focus audit should produce findings");
    assert_eq!(findings[0].category, "focus");
}

#[test]
fn test_clipping_audit_no_regions() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "import sys; print('hello'); sys.stdout.flush(); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::clipping_audit(sess);

    assert!(
        !findings.is_empty(),
        "clipping audit should produce findings"
    );
    assert_eq!(findings[0].category, "clipping");
}

#[test]
fn test_resize_audit_restores_dimensions() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "import sys; print('resize test'); sys.stdout.flush(); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let original_cols = sess.cols();
    let original_rows = sess.rows();
    let findings = tui_lab::audit::resize_audit(sess);

    assert!(!findings.is_empty(), "resize audit should produce findings");
    assert_eq!(findings[0].category, "resize");
    assert_eq!(sess.cols(), original_cols);
    assert_eq!(sess.rows(), original_rows);
}

#[test]
fn test_keyboard_audit_empty_session() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &["-c".to_string(), "import sys; sys.exit(0)".to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");

    let _ = mgr
        .resolve_mut(Some(&id))
        .unwrap()
        .wait(tui_lab::backend::WaitCond::ProcessExit, 2000);

    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::keyboard_audit(sess, 2);

    assert!(!findings.is_empty());
}
