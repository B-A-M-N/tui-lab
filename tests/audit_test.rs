// Tests for the active audit driver: keyboard, focus, resize, clipping audits.
//
// Migrated off the legacy `SessionManager` (audit P1-54): sessions now run
// through the actor-backed `SessionPool`, the production surface. Each audit
// driver runs as one job ON the session's actor thread via `with_session`.

use tui_lab::session::SessionPool;

async fn start(pool: &SessionPool, script: &str) -> String {
    pool.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
        "auto",
        "local",
    )
    .await
    .expect("start")
}

#[tokio::test]
async fn test_keyboard_audit_detects_states() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        "import sys; print('tab test'); sys.stdout.flush(); input()",
    )
    .await;

    let findings = pool
        .with_session(Some(&id), move |sess| {
            let mut graph = tui_lab::semantic::focus_graph::FocusGraph::new();
            tui_lab::audit::keyboard_audit(sess, 3, &mut graph)
        })
        .await
        .expect("job");

    assert!(
        !findings.is_empty(),
        "keyboard audit should produce findings"
    );
    assert_eq!(findings[0].category.as_str(), "keyboard");
    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn test_focus_audit_detects_focus() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        "import sys; print('focus test'); sys.stdout.flush(); input()",
    )
    .await;

    let findings = pool
        .with_session(Some(&id), tui_lab::audit::focus_audit)
        .await
        .expect("job");

    assert!(!findings.is_empty(), "focus audit should produce findings");
    assert_eq!(findings[0].category.as_str(), "focus");
    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn test_clipping_audit_no_regions() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        "import sys; print('hello'); sys.stdout.flush(); input()",
    )
    .await;

    let findings = pool
        .with_session(Some(&id), tui_lab::audit::clipping_audit)
        .await
        .expect("job");

    assert!(
        !findings.is_empty(),
        "clipping audit should produce findings"
    );
    assert_eq!(findings[0].category.as_str(), "clipping");
    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn test_resize_audit_restores_dimensions() {
    let pool = SessionPool::new();
    let id = start(
        &pool,
        "import sys; print('resize test'); sys.stdout.flush(); input()",
    )
    .await;

    let (findings, cols, rows) = pool
        .with_session(Some(&id), move |sess| {
            let _original_cols = sess.cols();
            let _original_rows = sess.rows();
            let findings = tui_lab::audit::resize_audit(sess);
            (findings, sess.cols(), sess.rows())
        })
        .await
        .expect("job");

    assert!(!findings.is_empty(), "resize audit should produce findings");
    assert_eq!(findings[0].category.as_str(), "resize");
    assert_eq!(cols, 80, "dimensions restored");
    assert_eq!(rows, 24, "dimensions restored");
    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn test_keyboard_audit_empty_session() {
    let pool = SessionPool::new();
    let id = start(&pool, "import sys; sys.exit(0)").await;

    pool.with_session(Some(&id), move |sess| {
        let _ = sess.wait(tui_lab::backend::WaitCond::ProcessExit, 2000);
        let mut graph = tui_lab::semantic::focus_graph::FocusGraph::new();
        tui_lab::audit::keyboard_audit(sess, 2, &mut graph)
    })
    .await
    .expect("job");

    pool.stop(&id).await.expect("stop");
}
