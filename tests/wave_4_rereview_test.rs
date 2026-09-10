//! Re-review Wave 4: the converged event history (items 15/16), declarative
//! event-predicate waits (item 17), and tmux attach (item 18).

use tui_lab::events::{project_history, BusSource, HistoryQuery};
use tui_lab::session::SessionPool;

async fn python_session(pool: &SessionPool, code: &str) -> String {
    let args: Vec<String> = vec!["-c".to_string(), code.to_string()];
    pool.start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start python session")
}

// ── Item 15/16: the history projection ───────────────────────────────────

/// Pure-function coverage of the projection: windowing, kind filtering, the
/// stable per-source counter, the served cursor, and the declared gap.
#[test]
fn history_projection_windows_filters_and_counts() {
    use tui_lab::events::{TerminalEvent, TerminalEventKind};
    let mk = |seq: u64, kind: TerminalEventKind| TerminalEvent {
        monotonic_ms: 0,
        seq,
        at: 1000 + seq,
        session: "s".into(),
        generation: 1,
        kind,
    };
    let events = vec![
        mk(1, TerminalEventKind::Bell),
        mk(2, TerminalEventKind::Output { byte_len: 10 }),
        mk(3, TerminalEventKind::Bell),
        mk(
            4,
            TerminalEventKind::NativeEvent {
                event: "focus:list".into(),
                target: "n1".into(),
            },
        ),
        mk(5, TerminalEventKind::Output { byte_len: 20 }),
    ];

    // Whole window: all five, native re-typed to the Native source (item 16).
    let all = project_history(&events, &HistoryQuery::default(), 0);
    assert_eq!(all.events.len(), 5);
    assert_eq!(
        all.events.iter().find(|e| e.seq == 4).map(|e| e.source),
        Some(BusSource::Native),
        "NativeEvent projects to the native source"
    );
    // Per-source counters count every event of the source (bells at store
    // positions 1,3 make the outputs 2,4) — computed over the WHOLE store,
    // not the served window, so they are stable across windows.
    let out_seqs: Vec<u64> = all
        .events
        .iter()
        .filter(|e| e.source == BusSource::Terminal)
        .filter(|e| {
            matches!(
                e.kind,
                tui_lab::events::BusEventKind::Terminal(TerminalEventKind::Output { .. })
            )
        })
        .map(|e| e.source_seq)
        .collect();
    assert_eq!(out_seqs, vec![2, 4], "per-source counters are stable");

    // Windowing: seq > 2.
    let since = project_history(
        &events,
        &HistoryQuery {
            since_seq: 2,
            ..Default::default()
        },
        0,
    );
    assert_eq!(
        since.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4, 5]
    );

    // Kind filter: bells only, counters still store-wide.
    let bells = project_history(
        &events,
        &HistoryQuery {
            event_types: vec!["bell".into()],
            ..Default::default()
        },
        0,
    );
    assert_eq!(bells.events.len(), 2);
    assert!(bells.events.iter().all(|e| matches!(
        e.kind,
        tui_lab::events::BusEventKind::Terminal(TerminalEventKind::Bell)
    )));

    // Limit truncation: cursor = last SERVED seq so paging never skips.
    let page1 = project_history(
        &events,
        &HistoryQuery {
            limit: Some(2),
            ..Default::default()
        },
        0,
    );
    assert_eq!(page1.events.len(), 2);
    assert_eq!(page1.cursor, 2);
    let page2 = project_history(
        &events,
        &HistoryQuery {
            since_seq: page1.cursor,
            limit: Some(2),
            ..Default::default()
        },
        0,
    );
    assert_eq!(
        page2.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4],
        "paging by the served cursor continues exactly where page 1 ended"
    );

    // Declared gap: reaching behind the eviction watermark.
    let gapped = project_history(
        &events,
        &HistoryQuery {
            since_seq: 0,
            ..Default::default()
        },
        2,
    );
    assert!(gapped.gap, "queries behind eviction must declare the gap");
    let fresh = project_history(
        &events,
        &HistoryQuery {
            since_seq: 2,
            ..Default::default()
        },
        2,
    );
    assert!(!fresh.gap, "a query entirely inside the window is complete");
}

/// End-to-end through a real session: act on a TUI, then read the history
/// and find the output/screen-changed events that the action produced,
/// with a cursor that advances cleanly.
#[tokio::test]
async fn session_history_serves_real_action_events() {
    let pool = SessionPool::new();
    // Print in two phases: the FIRST observation only seeds `last` (the
    // screen it settles on is the baseline), so the ScreenChanged event
    // comes from the transition between phase 1 and phase 2.
    let sid = python_session(
        &pool,
        "import time\nprint('hello'); time.sleep(1); print('world'); time.sleep(30)",
    )
    .await;
    let (events, evicted, after_empty) = pool
        .with_session(Some(&sid), move |sess| {
            let _ = sess.observe(300);
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let _ = sess.observe(300);

            let events = sess.all_events();
            // Serve through the projection with a cursor and confirm it advances.
            let batch = project_history(&events, &HistoryQuery::default(), sess.events_evicted());
            let after = project_history(
                &sess.all_events(),
                &HistoryQuery {
                    since_seq: batch.cursor,
                    ..Default::default()
                },
                0,
            );
            (events, sess.events_evicted(), after.events.is_empty())
        })
        .await
        .expect("history job");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, tui_lab::events::TerminalEventKind::Output { .. })),
        "a launched child that printed must produce output events"
    );
    assert!(
        events.iter().any(|e| matches!(
            e.kind,
            tui_lab::events::TerminalEventKind::ScreenChanged { .. }
        )),
        "the hello→world transition must be a screen_changed event"
    );
    let batch = project_history(&events, &HistoryQuery::default(), evicted);
    assert!(!batch.gap);
    assert_eq!(
        batch.cursor,
        batch.events.last().map(|e| e.seq).unwrap_or(0)
    );
    assert!(after_empty, "since the served cursor there is nothing new");
    pool.stop(&sid).await.ok();
}

/// Native events converge into the same history with the native source.
/// The app side is simulated exactly as a cooperative adapter does: write an
/// NDJSON event frame to the semantic channel file, then let the session
/// poll it in.
#[tokio::test]
async fn history_wire_kinds_and_native_source() {
    use std::io::Write;
    use tui_lab::events::TerminalEventKind;
    let pool = SessionPool::new();
    let sid = python_session(&pool, "print('x'); import time; time.sleep(30)").await;
    // Observe once so the queue seeds, then simulate one app event.
    pool.with_session(Some(&sid), move |sess| {
        let _ = sess.observe(300);
        let (_, path) = sess
            .native_channel()
            .env_pair()
            .expect("session has a semantic channel");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open channel");
        writeln!(
            f,
            r#"{{"v":1,"type":"event","event":"focus","target":"menu.file"}}"#
        )
        .expect("write frame");
    })
    .await
    .expect("seed job");
    pool.with_session(Some(&sid), move |sess| sess.poll_native())
        .await
        .expect("poll job");
    let events = pool
        .with_session(Some(&sid), move |sess| sess.all_events())
        .await
        .expect("events job");
    let names: Vec<&str> = events
        .iter()
        .map(|ev| match ev.kind {
            TerminalEventKind::NativeEvent { .. } => "native_event",
            _ => ev.kind.name(),
        })
        .collect();
    assert!(
        names.contains(&"native_event"),
        "the polled native event must be absorbed into history; got {names:?}"
    );
    // The converged vocabulary projects it under the native source.
    let batch = project_history(&events, &HistoryQuery::default(), 0);
    assert!(
        batch.events.iter().any(|e| e.source == BusSource::Native
            && matches!(
                e.kind,
                tui_lab::events::BusEventKind::Terminal(TerminalEventKind::NativeEvent { .. })
            )),
        "native events must carry the native source on the converged timeline"
    );
    // And the kind filter names it exactly as an agent would ask.
    let only_native = project_history(
        &events,
        &HistoryQuery {
            event_types: vec!["native_event".into()],
            ..Default::default()
        },
        0,
    );
    assert_eq!(only_native.events.len(), 1);
    pool.stop(&sid).await.ok();
}

// ── Item 17: declarative event-predicate waits ───────────────────────────

/// The predicate is conjunctive: kinds filter, `contains` narrows inside the
/// payload, `since_seq` demands something NEW.
#[test]
fn event_predicate_matches_conjunctively() {
    use tui_lab::events::TerminalEventKind;
    use tui_lab::mcp::params::EventPredicate;
    let mk = |seq: u64, kind: TerminalEventKind| tui_lab::events::TerminalEvent {
        monotonic_ms: 0,
        seq,
        at: 1000 + seq,
        session: "s".into(),
        generation: 1,
        kind,
    };
    let bell = mk(1, TerminalEventKind::Bell);
    let title = mk(
        2,
        TerminalEventKind::TitleChanged {
            title: "Editor — file.rs".into(),
        },
    );
    let native = mk(
        3,
        TerminalEventKind::NativeEvent {
            event: "focus".into(),
            target: "menu.file".into(),
        },
    );

    let kinds = |kinds: &[&str]| EventPredicate {
        kinds: kinds.iter().map(|s| s.to_string()).collect(),
        contains: None,
        since_seq: None,
    };
    assert!(kinds(&["bell"]).matches(&bell));
    assert!(!kinds(&["bell"]).matches(&title));
    assert!(kinds(&["bell", "title_changed"]).matches(&title));

    // `contains` looks in the payload; a bell has none, so it can never
    // satisfy a contains predicate (honest no-match, not vacuous).
    let needle = EventPredicate {
        kinds: vec![],
        contains: Some("file.rs".into()),
        since_seq: None,
    };
    assert!(needle.matches(&title));
    assert!(!needle.matches(&bell));
    let native_needle = EventPredicate {
        kinds: vec![],
        contains: Some("menu.file".into()),
        since_seq: None,
    };
    assert!(native_needle.matches(&native));

    // `since_seq` is exclusive.
    let fresh_only = EventPredicate {
        kinds: vec![],
        contains: None,
        since_seq: Some(2),
    };
    assert!(fresh_only.matches(&native));
    assert!(!fresh_only.matches(&bell), "seq 1 is behind the cursor");
    assert!(!fresh_only.matches(&title), "seq 2 is not > 2");
}

/// End-to-end: a child that prints after a delay; the event wait fires only
/// when the new output's events arrive, and a `since_seq`-anchored wait
/// ignores the past.
#[tokio::test]
async fn wait_event_fires_on_new_output_and_respects_since_seq() {
    use tui_lab::mcp::params::EventPredicate;
    let pool = SessionPool::new();
    let sid = python_session(
        &pool,
        "import time\nprint('first'); time.sleep(1); print('second'); time.sleep(30)",
    )
    .await;
    pool.with_session(Some(&sid), move |sess| {
        let _ = sess.observe(300);
        // The predicate: any new screen_changed event AFTER now.
        let pred = EventPredicate {
            kinds: vec!["screen_changed".into()],
            contains: None,
            since_seq: Some(sess.event_queue_last_seq()),
        };
        let out = tui_lab::execution::execute_wait_event(sess, &pred, 5000).expect("wait");
        assert!(
            out.met,
            "the delayed second print must produce a screen_changed event"
        );
        assert!(out.matched_seq > 0);

        // Re-waiting from the matched cursor finds nothing new and times out.
        let stale = EventPredicate {
            kinds: vec!["screen_changed".into()],
            contains: None,
            since_seq: Some(out.matched_seq),
        };
        let out2 = tui_lab::execution::execute_wait_event(sess, &stale, 400).expect("wait");
        assert!(!out2.met, "no further screen change is pending");
    })
    .await
    .expect("wait job");
    pool.stop(&sid).await.ok();
}

// ── Item 18: tmux attach ─────────────────────────────────────────────────

/// Attach to a REAL tmux pane running a live TUI: observe its screen, send
/// keys, see the reaction, then detach — and confirm the pane (the user's
/// TUI) outlives the attach session.
#[test]
fn tmux_attach_observes_and_drives_a_live_pane() {
    // A tmux server/socket can be unavailable even when the binary exists
    // (restricted sandboxes). Treat that as an optional integration skip,
    // matching the backend conformance suite.
    let probe = std::process::Command::new("tmux")
        .args(["list-sessions"])
        .output();
    if probe.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: tmux unavailable — skipping tmux attach E2E");
        return;
    }
    let sess_name = format!("tuilab-test-{}", std::process::id());
    let out = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &sess_name,
            "-x",
            "80",
            "-y",
            "24",
            "python3",
            "-u",
            "-c",
            r#"
import sys, time
print("READY")
while True:
    line = sys.stdin.readline()
    if not line:
        break
    cmd = line.strip()
    if cmd == "quit":
        print("BYE")
        break
    print(f"GOT:{cmd}")
    sys.stdout.flush()
"#,
        ])
        .output()
        .expect("spawn tmux session");
    assert!(
        out.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Attach through the public pool API (the same path the MCP handler
    // uses).
    let pool = tui_lab::session::SessionPool::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let sid = rt.block_on(async {
        let id = pool
            .attach_tmux(&format!("{sess_name}:0.0"), 80, 24)
            .await
            .expect("attach");
        // Observe: the READY banner must be visible through capture-pane.
        let text = pool
            .with_session(Some(&id), |s| {
                let screen = s.observe(150).expect("observe");
                screen.viewport_text.join("\n")
            })
            .await
            .expect("actor");
        assert!(
            text.contains("READY"),
            "the attach must see the pane's banner, got: {text:?}"
        );
        // Drive: send "hello\n" and wait for the echo.
        pool.with_session(Some(&id), |s| {
            use tui_lab::backend::{Input, KeyCode, KeyEvent};
            s.send(Input::Keys(vec![
                KeyEvent::new(KeyCode::Char('h')),
                KeyEvent::new(KeyCode::Char('e')),
                KeyEvent::new(KeyCode::Char('l')),
                KeyEvent::new(KeyCode::Char('l')),
                KeyEvent::new(KeyCode::Char('o')),
                KeyEvent::new(KeyCode::Enter),
            ]))
            .expect("send");
            use tui_lab::backend::WaitCond;
            let out = s
                .wait(WaitCond::Text("GOT:hello".into()), 3000)
                .expect("wait");
            assert!(out.met, "the pane must react to the injected keys");
            s.observe(50).expect("observe").viewport_text.join("\n")
        })
        .await
        .expect("actor2")
    });
    assert!(
        sid.contains("GOT:hello"),
        "the observed screen must show the app's reaction: {sid}"
    );
    // Detach (stop): the pane must SURVIVE — the TUI predates us.
    rt.block_on(async {
        pool.with_session(Some(pool.active_id().as_deref().unwrap_or("")), |s| {
            s.stop().expect("stop");
        })
        .await
        .expect("actor3");
    });
    let alive = std::process::Command::new("tmux")
        .args(["has-session", "-t", &sess_name])
        .output()
        .expect("has-session");
    assert!(
        alive.status.success(),
        "detaching must NOT kill the user's pane (item 18)"
    );
    // Clean up the test's own tmux session.
    std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sess_name])
        .output()
        .ok();
}
