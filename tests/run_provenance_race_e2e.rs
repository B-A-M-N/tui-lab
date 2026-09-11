//! Beta-audit P0-6: run-switch provenance — evidence authorized under one
//! run must never land in another.
//!
//! The race: `with_sess` authorizes a job against run A, the actor job
//! runs, `tui_run new` swaps the shared RunContext to B, the job's
//! evidence commit then locks whatever context is current. Without the
//! commit-time ticket check the transaction silently enters B — foreign
//! evidence spilling across a run switch.
//!
//! These tests are deterministic: the commit-time verify is exercised
//! directly (drive_pipeline with a ticket captured under A, run swapped
//! to B before the commit), so no timing luck is involved.

use tui_lab::session::SessionPool;

fn raw_ready_args(marker: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("import sys,tty; tty.setraw(0); print('{marker}'); sys.stdin.buffer.read(1)"),
    ]
}

fn key_spec<'a>(
    action: &'a tui_lab::execution::CanonicalAction,
    ticket: tui_lab::execution::RunTicket,
) -> tui_lab::execution::CoreDriveSpec<'a> {
    tui_lab::execution::CoreDriveSpec {
        action,
        quiet_ms: 120,
        budget_ms: 1200,
        no_wait: false,
        visibility: tui_lab::execution::InputVisibility::Normal,
        completion: tui_lab::capture::CompletionPolicy::StableScreen,
        guard: None,
        scenario: None,
        origin: tui_lab::execution::DriveOrigin::Act,
        ticket,
    }
}

fn key_x() -> tui_lab::execution::CanonicalAction {
    tui_lab::execution::CanonicalAction::Key {
        key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
    }
}

/// The headline race, made deterministic: the ticket is captured under
/// run A; the run context is swapped to B before the evidence commit; the
/// commit must REFUSE — the transaction lands in neither A's ledger
/// (dropped, reported) nor B's (never spills).
#[tokio::test]
async fn run_switch_between_authorization_and_commit_drops_evidence() {
    let pool = SessionPool::new();
    let id = pool
        .start(
            "python3",
            &raw_ready_args("RACE READY"),
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
        let run_a =
            std::sync::Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
        // "Authorization": capture the ticket under A (what with_sess's
        // guard window would do).
        let ticket = tui_lab::execution::RunTicket::capture(&run_a);

        // Simulate `tui_run new` mid-flight: the shared Arc now points at
        // run B (in production the handler clones the Arc and lifecycle
        // replaces the pointee).
        let run_b = tui_lab::run::RunContext::ephemeral();
        *run_a.lock().unwrap() = run_b;
        let b_id = run_a.lock().unwrap().id().to_string();

        // The in-flight act commits now — against B's context, with A's
        // ticket. It must error, and the error must name both runs.
        let action = key_x();
        let err = match tui_lab::execution::drive_pipeline(sess, &run_a, key_spec(&action, ticket))
        {
            Ok(_) => panic!("run-switched commit must fail"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("run switched under an in-flight operation"),
            "the refusal names the race: {msg}"
        );
        assert!(
            msg.contains(&b_id),
            "the refusal names the run it would NOT spill into: {msg}"
        );

        // B's ledger holds NOTHING from the in-flight act.
        assert_eq!(
            run_a.lock().unwrap().transactions().len(),
            0,
            "no transaction may enter the run it was not authorized under"
        );
    })
    .await
    .expect("race job");
}

/// Control: a ticket captured under the CURRENT run commits normally —
/// the guard is not an over-refusal that breaks the healthy path.
#[tokio::test]
async fn matching_ticket_commits_normally() {
    let pool = SessionPool::new();
    let id = pool
        .start(
            "python3",
            &raw_ready_args("MATCH READY"),
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
        let run = std::sync::Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
        let ticket = tui_lab::execution::RunTicket::capture(&run);
        let action = key_x();
        let outcome = tui_lab::execution::drive_pipeline(sess, &run, key_spec(&action, ticket))
            .expect("matching ticket commits");
        assert!(
            outcome.health.healthy(),
            "healthy path unaffected: {:?}",
            outcome.health.failures()
        );
        assert_eq!(
            run.lock().unwrap().transactions().len(),
            1,
            "the transaction landed in the authorized run"
        );
    })
    .await
    .expect("match job");
}

/// The fold path drops too: events stay in the SESSION queue when the run
/// switched, so a later fold under the CURRENT run picks them up — nothing
/// is misrouted and nothing is lost.
#[tokio::test]
async fn run_switch_drops_the_fold_without_losing_events() {
    let pool = SessionPool::new();
    let id = pool
        .start(
            "python3",
            &raw_ready_args("FOLD READY"),
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("start");

    pool.with_session(Some(&id.clone()), move |sess| {
        let run_a =
            std::sync::Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
        let ticket = tui_lab::execution::RunTicket::capture(&run_a);
        // Swap to B, then drive with A's ticket: the commit refuses (as in
        // the headline test) BEFORE the fold, so the act's events remain in
        // the session queue.
        *run_a.lock().unwrap() = tui_lab::run::RunContext::ephemeral();
        let action = key_x();
        let persist_key = format!("persistence:{id}");
        assert!(
            tui_lab::execution::drive_pipeline(sess, &run_a, key_spec(&action, ticket)).is_err()
        );

        // A stale-ticket fold is a no-op: B's persistence cursor does not
        // move. The stale ticket names A — captured against a context that
        // is then swapped away, exactly the in-flight authorization shape.
        let stale_ticket = {
            let holder = std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral());
            let t = tui_lab::execution::RunTicket::capture(&holder);
            *holder.lock().unwrap() = tui_lab::run::RunContext::ephemeral();
            t
        };
        let before = run_a.lock().unwrap().event_cursor(&persist_key);
        tui_lab::execution::fold_session_events(sess, &run_a, &stale_ticket);
        assert_eq!(
            run_a.lock().unwrap().event_cursor(&persist_key),
            before,
            "a stale-ticket fold must not advance the current run's cursor"
        );

        // A fold under the CURRENT run (fresh ticket) recovers the queued
        // events — nothing was lost by the drop.
        let fresh = tui_lab::execution::RunTicket::capture(&run_a);
        tui_lab::execution::fold_session_events(sess, &run_a, &fresh);
        let after = run_a.lock().unwrap().event_cursor(&persist_key);
        assert!(
            after.is_some() && after != before,
            "the queued events are recovered under the current run: {after:?}"
        );
    })
    .await
    .expect("fold race job");
}

// ── Beta-audit P0.3: the typed sink ops refuse cross-run bookkeeping ──
//
// The sink is now the one place handlers get run-side bookkeeping.
// Every typed op must verify the ticket like `commit` does: a record
// authorized under run A never lands in run B (dropped, reported as
// `false`), and the same op under the RIGHT run succeeds.

mod sink_bookkeeping {
    use std::sync::{Arc, Mutex};
    use tui_lab::execution::RunEvidenceSink;
    use tui_lab::run::RunContext;

    /// A run Arc swapped out from under a sink: the sink's ticket names
    /// run A; the Arc now holds run B.
    fn swapped() -> (Arc<Mutex<RunContext>>, RunEvidenceSink) {
        let run_a = Arc::new(Mutex::new(RunContext::ephemeral()));
        let sink = RunEvidenceSink::capture(&run_a);
        // The same deliberate-loss swap `tui_run new discard=true`
        // performs.
        *run_a.lock().unwrap() = RunContext::ephemeral();
        (run_a, sink)
    }

    #[tokio::test]
    async fn every_typed_op_refuses_after_a_run_switch() {
        let (_run, sink) = swapped();
        assert!(
            !sink.record_event("sess-x", "probe"),
            "record_event must refuse"
        );
        assert!(!sink.record_scenario_act("sess-x", 0, serde_json::json!({})));
        assert!(!sink.record_scenario_act_sensitive(
            "sess-x",
            0,
            serde_json::json!({}),
            "text",
            tui_lab::scenario::model::SensitiveKind::Secret,
            4
        ));
        assert!(!sink.record_scenario_assert("sess-x", 0, serde_json::json!({})));
        assert!(!sink.record_scenario_wait("sess-x", 0, serde_json::json!({})));
        assert!(!sink.record_scenario_intent("sess-x", 0, serde_json::json!({})));
        let local_graph = tui_lab::exploration::state_graph::StateGraph::new(Default::default());
        assert!(!sink.merge_state_graph(&local_graph));
        let local_focus = tui_lab::semantic::focus_graph::FocusGraph::new();
        assert!(!sink.merge_focus_graph(&local_focus));
        assert!(sink.with_run(|_run| 0u8).is_none(), "with_run must refuse");
    }

    #[tokio::test]
    async fn typed_ops_succeed_under_the_live_run() {
        let run = Arc::new(Mutex::new(RunContext::ephemeral()));
        let sink = RunEvidenceSink::capture(&run);
        assert!(sink.record_event("sess-x", "probe"));
        // Graph merges land in the run's graphs — observable through
        // the same verified hatch via status counts.
        let local = tui_lab::exploration::state_graph::StateGraph::new(Default::default());
        let mut focus = tui_lab::semantic::focus_graph::FocusGraph::new();
        // A focus transition (from -> to via Tab) is one observable
        // edge.
        focus.record_edge("ctrl-a", "ctrl-b", "tab", None);
        sink.merge_focus_graph(&focus);
        sink.merge_state_graph(&local);
        let edges = sink
            .with_run(|r| r.graphs().focus_graph.summary()["edges"].clone())
            .expect("live run");
        assert_eq!(
            edges,
            serde_json::json!(1),
            "the merged focus edge is in the ticketed run's graph"
        );
    }
}
