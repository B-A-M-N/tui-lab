//! End-to-end integration test: launch the curses dialog fixture, observe,
//! check semantic inference, act (Enter), and observe the resulting transition.
//! This exercises the entire runtime path (PTY -> vt100 -> screen -> semantic)
//! without going through MCP stdio.
//!
//! Defects fixed per the audit (items 79/80):
//!   * assertions no longer silently skip on a `None` observation — required
//!     behavior uses `.expect(...)`.
//!   * fixed `thread::sleep` waits are replaced by state-aware `wait(...)`
//!     calls, which both speeds up the test and exercises the synchronization
//!     layer that the audit flags as broken.
//!
//! Migrated off the legacy `SessionManager` (audit P1-54): the actor-backed
//! `SessionPool` is the production surface.

use tui_lab::backend::{Input, KeyCode, KeyEvent};
use tui_lab::semantic;
use tui_lab::session::SessionPool;

fn fixture() -> String {
    // absolute path to the dialog fixture checked into the repo
    let here = std::env::current_dir().unwrap();
    here.join("fixtures")
        .join("dialog_tui.py")
        .to_string_lossy()
        .to_string()
}

#[tokio::test]
async fn e2e_launch_observe_semantic_act() {
    let pool = SessionPool::new();
    let id = pool
        .start("python3", &[fixture()], None, &[], 80, 24, "auto", "local")
        .await
        .expect("session start");

    // Wait for the dialog to actually draw (state-aware, not a fixed sleep).
    {
        let met = pool
            .with_session(Some(&id), {
                let id = id.clone();
                move |sess| {
                    let _ = &id;
                    sess.wait(tui_lab::backend::WaitCond::Text("Settings".into()), 5000)
                        .map(|out| out.met)
                        .unwrap_or(false)
                }
            })
            .await
            .expect("wait job");
        assert!(met, "dialog title never appeared");
    }

    let (text, region_title_ok, has_save, save_source) = pool
        .with_session(Some(&id), move |sess| {
            let screen = sess.observe(80).expect("observe");
            let text = screen.text_view();
            let sem = semantic::analyze(&screen);
            let region_ok = sem
                .regions
                .first()
                .map(|r| r.title.as_deref() == Some("Settings") && r.confidence.score > 0.5)
                .unwrap_or(false);
            let save = sem.controls.iter().find(|c| c.label == "Save");
            (
                text,
                region_ok,
                save.is_some(),
                save.map(|s| s.confidence.source.to_string()),
            )
        })
        .await
        .expect("observe job");

    assert!(text.contains("Settings"), "dialog title missing: {text}");
    assert!(
        text.contains("Host:") || text.contains("Host"),
        "host field missing"
    );
    assert!(text.contains("Port:"), "port field missing");

    // Semantic inference: a bordered region titled Settings + a Save button.
    assert!(region_title_ok, "expected the bordered Settings region");
    assert!(has_save, "expected a Save button control");
    assert_eq!(save_source.as_deref(), Some("inferred"));

    // Act: press Enter (the fixture writes "saved." on keypress).
    pool.with_session(Some(&id), move |sess| {
        sess.send(Input::Key(KeyEvent::new(KeyCode::Enter)))
            .expect("send enter")
    })
    .await
    .expect("act job");

    // Wait for the resulting "saved." text (state-aware).
    {
        let met = pool
            .with_session(Some(&id), move |sess| {
                sess.wait(tui_lab::backend::WaitCond::Text("saved.".into()), 5000)
                    .map(|out| out.met)
                    .unwrap_or(false)
            })
            .await
            .expect("wait job");
        assert!(met, "Enter did not produce 'saved.' on screen");
    }
    let after_text = pool
        .with_session(Some(&id), move |sess| {
            sess.observe(80)
                .map(|s| s.text_view())
                .unwrap_or_else(|e| panic!("observe after: {e}"))
        })
        .await
        .expect("observe job");
    assert!(
        after_text.contains("saved."),
        "Enter did not produce 'saved.' on screen"
    );

    pool.stop(&id).await.expect("stop");
}

#[tokio::test]
async fn e2e_transition_diff_after_key() {
    let pool = SessionPool::new();
    let id = pool
        .start("python3", &[fixture()], None, &[], 80, 24, "auto", "local")
        .await
        .expect("start");
    {
        let met = pool
            .with_session(Some(&id), move |sess| {
                sess.wait(tui_lab::backend::WaitCond::Text("Settings".into()), 5000)
                    .map(|out| out.met)
                    .unwrap_or(false)
            })
            .await
            .expect("wait job");
        assert!(met);
    }
    pool.with_session(Some(&id), move |sess| {
        sess.observe(120).expect("observe");
    })
    .await
    .expect("observe job");
    // Send a printable char (the fixture ignores it on screen but the action
    // still exercises send_input + vt100 round-trip + transition diff).
    pool.with_session(Some(&id), move |sess| {
        sess.send(Input::Key(KeyEvent::new(KeyCode::Char('x'))))
            .expect("send");
        sess.observe(120).expect("observe");
    })
    .await
    .expect("act job");
    // Structural hash must be stable/idempotent for identical screens.
    let a = pool
        .with_session(Some(&id), move |sess| sess.last().cloned())
        .await
        .expect("snapshot job")
        .expect("first snapshot present");
    pool.with_session(Some(&id), move |sess| {
        sess.observe(120).expect("observe");
    })
    .await
    .expect("observe job");
    let b = pool
        .with_session(Some(&id), move |sess| sess.observe(120).ok())
        .await
        .expect("observe job")
        .expect("second snapshot present");
    // No volatile content in this fixture, so structure hashes should match.
    assert_eq!(
        a.structure_hash, b.structure_hash,
        "structure hash not stable"
    );
    pool.stop(&id).await.expect("stop");
}
