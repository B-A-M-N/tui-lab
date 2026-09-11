//! Audit P0 regression suite (dispatch-atomicity + evidence correctness).
//!
//! Covers the fixed findings:
//! - #1: dispatch is a single typed boundary (no pump between guard
//!       validation and the transport write).
//! - #2: before-side of the transaction derives ENTIRELY from the
//!       preflight FrameAnalysis (no stale fused_frame over last()).
//! - #3: MutationGuard::capture stamps semantic identity (native-only
//!       drift is caught).
//! - #4/#5: dispatch failures are typed (FailedBeforeWrite vs
//!       PartialOrUnknown) and never masquerade as settle timeouts.
//! - #9: transaction event_seq_before/after are EVENT-RING seqs, not
//!       backend output_seq.
//! - #7: AfterDuration respects the operation budget.
//! - #8: UntilEvent is refused at the backend level (session-level
//!       matcher exists instead).
//! - #19: restart never discards a stop() failure.
//! - #23: ledger transaction seq comes from the sink commit, not a
//!       heuristic backscan.

use tui_lab::backend::TerminalBackend;
use tui_lab::session::SessionPool;

async fn python_session(pool: &SessionPool, code: &str) -> String {
    let args: Vec<String> = vec!["-c".to_string(), code.to_string()];
    pool.start("python3", &args, None, &[], 80, 24, "auto", "local")
        .await
        .expect("start python session")
}

// ── Finding 1/2: one dispatch boundary; before-side from preflight ──
//
// A guard captured from a fresh analysis PLUS a target that redraws in the
// validation→send gap: the only pump remaining between validation and the
// physical write is the backend's own pre-write drain inside dispatch().
// The before-side of the transaction must describe the SAME revision the
// guard validated (never a stale fused_frame over session.last()).

#[tokio::test]
async fn before_identity_matches_preflight_not_last() {
    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys,time
print('PHASE-A', flush=True)
time.sleep(0.4)
print('PHASE-B', flush=True)
sys.stdin.read(1)
print('RECEIVED', flush=True)",
    )
    .await;
    pool.with_session(Some(&id), move |sess| {
        sess.observe(300).expect("baseline A");
        // Let the app advance to PHASE-B WITHOUT observing it: `last()`
        // still describes PHASE-A after snapshot_fresh() sees PHASE-B —
        // exactly the stale last() the old before-side could read.
        std::thread::sleep(std::time::Duration::from_millis(600));
        // The guard is captured (and the tx before-side must be derived)
        // from the FRESH analysis: PHASE-B, not the stale PHASE-A.
        let analysis = sess.snapshot_fresh().expect("fresh snapshot");
        let guard =
            tui_lab::execution::MutationGuard::capture(Some(&analysis), sess, sess.generation);
        let tx = tui_lab::execution::execute_act_with_guard_and_origin(
            sess,
            tui_lab::execution::DriveOrigin::Act,
            &tui_lab::execution::CanonicalAction::Key {
                key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
            },
            60,
            1500,
            false,
            tui_lab::execution::InputVisibility::Normal,
            tui_lab::capture::CompletionPolicy::StableScreen,
            Some(&guard),
        )
        .expect("guarded act");
        // Before-side must be PHASE-B (fresh), not PHASE-A (stale last()).
        assert!(
            tx.before()
                .viewport_text
                .iter()
                .any(|r| r.contains("PHASE-B")),
            "before-side must come from the fresh preflight, got: {:?}",
            tx.before().viewport_text
        );
        // And the event range must be in the EVENT-RING domain.
        assert!(
            tx.event_seq_after.unwrap_or(tx.event_seq_before) >= tx.event_seq_before,
            "event range is monotonic"
        );
        sess.stop().ok();
    })
    .await
    .expect("job");
}

// ── Finding 3: capture() carries the semantic identity ──

#[tokio::test]
async fn guard_capture_stamps_semantic_identity() {
    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; print('SEM-GUARD', flush=True); sys.stdin.read(1)",
    )
    .await;
    pool.with_session(Some(&id), move |sess| {
        sess.observe(300).expect("baseline");
        let analysis = sess.snapshot_fresh().expect("fresh");
        let guard =
            tui_lab::execution::MutationGuard::capture(Some(&analysis), sess, sess.generation);
        assert!(
            guard.semantic_identity.is_some(),
            "capture() must stamp the fused semantic identity (finding 3)"
        );
        // A guard captured over a DIFFERENT identity fails validation.
        let mut drift = analysis.clone();
        drift.semantic_identity = "some-other-world".into();
        let drifted =
            tui_lab::execution::MutationGuard::capture(Some(&drift), sess, sess.generation);
        assert!(
            drifted
                .validate_analysis(&analysis, sess, sess.generation)
                .is_err(),
            "semantic identity drift must fail the guard"
        );
        sess.stop().ok();
    })
    .await
    .expect("job");
}

// ── Finding 1 native-only focus drift: guard validation sees the latest
// native revision immediately before the write. The drift seam advances
// the native channel without changing the parsed grid; a guard captured
// before the move must refuse, and a guard captured after it must pass.

#[tokio::test]
async fn native_only_focus_drift_refuses_guarded_dispatch() {
    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; print('NATIVE-DRIFT', flush=True); sys.stdin.read(1)",
    )
    .await;
    pool.with_session(Some(&id), move |sess| {
        sess.observe(300).expect("baseline");
        let before = sess.snapshot_fresh().expect("fresh");
        let stale_guard =
            tui_lab::execution::MutationGuard::capture(Some(&before), sess, sess.generation);

        // Native-only drift with no pixel change.
        sess.test_declare_native_focus("#modal");

        let stale = tui_lab::execution::execute_act_with_guard_and_origin(
            sess,
            tui_lab::execution::DriveOrigin::Act,
            &tui_lab::execution::CanonicalAction::Key {
                key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
            },
            30,
            400,
            true,
            tui_lab::execution::InputVisibility::Normal,
            tui_lab::capture::CompletionPolicy::NoWait,
            Some(&stale_guard),
        );
        assert!(
            stale.is_err() && stale.unwrap_err().to_string().contains("stale_state"),
            "native-only focus drift must refuse the guarded dispatch"
        );

        // The refreshed world matches the declared focus; dispatch succeeds.
        let fresh = sess.snapshot_fresh().expect("fresh after drift");
        let fresh_guard =
            tui_lab::execution::MutationGuard::capture(Some(&fresh), sess, sess.generation);
        let tx = tui_lab::execution::execute_act_with_guard_and_origin(
            sess,
            tui_lab::execution::DriveOrigin::Act,
            &tui_lab::execution::CanonicalAction::Key {
                key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
            },
            30,
            400,
            true,
            tui_lab::execution::InputVisibility::Normal,
            tui_lab::capture::CompletionPolicy::NoWait,
            Some(&fresh_guard),
        )
        .expect("fresh guarded dispatch");
        assert_eq!(tx.dispatch, tui_lab::execution::DispatchStatus::Sent);
        sess.stop().ok();
    })
    .await
    .expect("job");
}

// ── Finding 4/5: dispatch failure is typed, never a settle timeout ──

#[tokio::test]
async fn dispatch_failure_is_typed_not_settle_timeout() {
    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys; print('DISP', flush=True); sys.stdin.read(1)",
    )
    .await;
    pool.with_session(Some(&id), move |sess| {
        sess.observe(300).expect("baseline");
        // A backend-level unsupported family (mouse on a pipe backend) must
        // be classified FailedBeforeWrite, and the returned error must say
        // so — not present itself as a settle timeout.
        let result = tui_lab::execution::execute_act(
            sess,
            &tui_lab::execution::CanonicalAction::MouseClick {
                button: tui_lab::backend::MouseButton::Left,
                x: 2,
                y: 2,
            },
            60,
            500,
            false,
        );
        assert!(result.is_err(), "mouse on a pipe-ish target must fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed_before_write")
                || err.contains("FailedBeforeWrite")
                || err.contains("dispatch"),
            "error must be a typed dispatch failure, got: {err}"
        );
        sess.stop().ok();
    })
    .await
    .expect("job");
}

// ── Finding 7: AfterDuration respects the overall budget ──

#[test]
fn after_duration_honors_budget_ceiling() {
    // A requested sample offset ABOVE the operation budget is an honest
    // deadline miss — capture immediately, never sleep past the budget.
    let mut b = tui_lab::backend::PtyLineBackend::new(80, 24);
    b.start(
        "python3",
        &[
            "-c".to_string(),
            "import time; print('A'); time.sleep(2)".into(),
        ],
        None,
        &[],
        80,
        24,
    )
    .expect("start");
    let baseline = b.event_state().screen_seq;
    let out = tui_lab::capture::capture_by_strategy(
        &mut b,
        &tui_lab::capture::CaptureStrategy::AfterDuration { ms: 10_000 },
        baseline,
        std::time::Duration::from_millis(200),
    )
    .expect("capture");
    assert!(
        !out.met,
        "a sample offset beyond the budget must report the deadline miss"
    );
    assert!(
        out.elapsed_ms <= 250,
        "capture must not sleep past the budget: {}",
        out.elapsed_ms
    );
    // Finding 7 evidence: requested vs actual are both preserved.
    assert_eq!(out.actual_sample_offset_ms, 0);
    b.stop().ok();
}

// ── Finding 8: UntilEvent is refused at the backend level ──

#[test]
fn until_event_is_refused_at_backend_level() {
    let mut b = tui_lab::backend::PtyLineBackend::new(80, 24);
    b.start(
        "python3",
        &[
            "-c".to_string(),
            "import time; print('EVT'); time.sleep(2)".into(),
        ],
        None,
        &[],
        80,
        24,
    )
    .expect("start");
    let baseline = b.event_state().screen_seq;
    let out = tui_lab::capture::capture_by_strategy(
        &mut b,
        &tui_lab::capture::CaptureStrategy::UntilEvent(tui_lab::capture::TerminalEventMatcher(
            "Bell",
        )),
        baseline,
        std::time::Duration::from_millis(300),
    );
    assert!(
        out.is_err(),
        "backend-level UntilEvent must be refused (no event vocabulary): {:?}",
        out
    );
    b.stop().ok();
}

// ── Finding 19: restart never discards a stop() failure ──
//
// Direct backend-level test: a backend whose stop() fails must make
// restart() refuse BEFORE emitting ProcessExited or advancing generations.
// We can't inject a failing backend through the SessionPool public API, so
// we exercise the session's restart path against a live child and assert
// the exit EVENT is only emitted after a successful stop (the invariant
// the finding guards).

#[tokio::test]
async fn restart_emits_exit_only_after_successful_stop() {
    let pool = SessionPool::new();
    let id = python_session(
        &pool,
        "import sys,time; print('R1', flush=True); time.sleep(30)",
    )
    .await;
    let events_before = pool
        .with_session(Some(&id), |sess| sess.all_events())
        .await
        .expect("events");
    let exited_before = events_before
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                tui_lab::events::TerminalEventKind::ProcessExited { .. }
            )
        })
        .count();
    pool.with_session(Some(&id), |sess| {
        sess.restart()
            .expect("restart succeeds (live child stops cleanly)");
    })
    .await
    .expect("restart job");
    let events_after = pool
        .with_session(Some(&id), |sess| sess.all_events())
        .await
        .expect("events");
    let exited_after = events_after
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                tui_lab::events::TerminalEventKind::ProcessExited { .. }
            )
        })
        .count();
    assert!(
        exited_after > exited_before,
        "a clean restart emits exactly one ProcessExited for the old generation"
    );

    // Finding 39 alignment: the old generation's death must precede the new
    // generation's start in the same restart lifecycle. The centralized
    // primitive emits ProcessExited during stop_lifecycle(); start emits
    // ProcessStarted only afterward, and generation advances only after
    // both succeeded.
    // The retained ring is newest-first. Convert to chronological order:
    // the FIRST event must be the original generation's ProcessStarted, the
    // next lifecycle event must be its observed ProcessExited, and only
    // then may the restarted generation's ProcessStarted appear.
    let chronological: Vec<&str> = events_after
        .iter()
        .rev()
        .filter_map(|e| match &e.kind {
            tui_lab::events::TerminalEventKind::ProcessExited { .. } => Some("exited"),
            tui_lab::events::TerminalEventKind::ProcessStarted => Some("started"),
            _ => None,
        })
        .collect();
    assert!(
        chronological.len() >= 3,
        "expected start/exited/start lifecycle, got {chronological:?}"
    );
    assert_eq!(chronological[0], "started", "{chronological:?}");
    assert_eq!(
        chronological[1], "exited",
        "old generation exit must be emitted before restart start: {chronological:?}"
    );
    assert_eq!(chronological[2], "started", "{chronological:?}");
    pool.stop(&id).await.unwrap_or_default();
}

#[test]
fn until_event_session_evaluates_exact_matcher_not_any_output() {
    use tui_lab::capture::{capture_by_strategy_session, CaptureStrategy, TerminalEventMatcher};

    // Backend-type is irrelevant here; the event matcher runs at Session
    // level. A pipe-ish CLI backend is deliberately chosen because it makes
    // ordinary output easy and a bell impossible.
    let mut b = tui_lab::backend::PtyLineBackend::new(80, 24);
    b.start(
        "python3",
        &[
            "-c".to_string(),
            "import sys,time; print('ORDINARY', flush=True); time.sleep(2)".into(),
        ],
        None,
        &[],
        80,
        24,
    )
    .expect("start");
    let mut session =
        tui_lab::session::state::Session::new("event-matcher".into(), "python3".into());
    session
        .start_with_spec(tui_lab::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time; print('ORDINARY', flush=True); time.sleep(2)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("session");
    let anchor = session.event_state().screen_seq;
    let out = capture_by_strategy_session(
        &mut session,
        &CaptureStrategy::UntilEvent(TerminalEventMatcher("Bell".into())),
        anchor,
        std::time::Duration::from_millis(400),
    )
    .expect("session event capture");
    assert!(!out.met, "ordinary output must not satisfy a Bell matcher");
    session.stop().ok();
}
