//! Beta-audit P0-7: origin parity — the "one canonical machine-driving
//! pipeline" must be universal. Every driving origin (act, explore,
//! audit, repro, conformance, probe, scenario) commits through the same
//! evidence sink, so each gets: a typed origin on the ledger row, citable
//! frames, a reconstructable transaction, and folded session events.
//! Before the sink, exploration/audit/repro/conformance called
//! `execute_act_as` directly and silently dropped subsets of this.

use std::sync::{Arc, Mutex};

use tui_lab::execution::{CoreDriveSpec, DriveOrigin, InputVisibility, RunEvidenceSink};
use tui_lab::run::RunContext;
use tui_lab::session::SessionPool;

fn raw_ready_args(marker: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("import sys,tty; tty.setraw(0); print('{marker}'); sys.stdin.buffer.read(1)"),
    ]
}

fn key_x() -> tui_lab::execution::CanonicalAction {
    tui_lab::execution::CanonicalAction::Key {
        key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
    }
}

async fn spawn(pool: &SessionPool, marker: &str) -> String {
    pool.start(
        "python3",
        &raw_ready_args(marker),
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

/// The origin set the sink must serve. Each entry drives one action
/// through `execute_act_as` (the primitive every legacy driver uses)
/// with a sink installed, and asserts the FULL evidence contract.
#[tokio::test]
async fn every_origin_gets_the_full_evidence_contract() {
    let pool = SessionPool::new();

    let origins: Vec<(DriveOrigin, &str)> = vec![
        (DriveOrigin::Act, "ACT"),
        (DriveOrigin::Intent, "INTENT"),
        (DriveOrigin::Scenario, "SCEN"),
        (DriveOrigin::Explore, "EXPLORE"),
        (DriveOrigin::ExploreSemantic, "EXSEM"),
        (DriveOrigin::Repro, "REPRO"),
        (DriveOrigin::Audit, "AUDIT"),
        (DriveOrigin::Conformance, "CONF"),
        (DriveOrigin::Probe, "PROBE"),
        (DriveOrigin::Scaffold, "SCAF"),
    ];

    for (origin, marker) in origins {
        let id = spawn(&pool, marker).await;
        pool.with_session(Some(&id), move |sess| {
            let run = Arc::new(Mutex::new(RunContext::ephemeral()));
            let sink = RunEvidenceSink::capture(&run);
            let _ticket = sink.ticket().clone();

            // Drive one act through the canonical executor with the sink
            // installed — what every unified driver path now does.
            sess.install_evidence_sink(sink);
            let tx = tui_lab::execution::execute_act_as(sess, origin, &key_x(), 120, 1200, false)
                .unwrap_or_else(|e| panic!("{origin:?}: drive failed: {e:#}"));
            sess.take_evidence_sink();

            // 1. Typed origin: recorded evidence, not an inference.
            assert_eq!(
                tx.origin,
                Some(origin),
                "{origin:?}: the transaction carries its own origin"
            );

            // 2+3. Citable frames + reconstructable transaction, in the
            // run ledger — committed by the SINK during execution, before
            // the driver ever looks.
            let run = run.lock().unwrap();
            let txs = run.transactions();
            assert_eq!(
                txs.len(),
                1,
                "{origin:?}: exactly one ledger row for one driven act"
            );
            let row = &txs[0];
            assert_eq!(
                row.origin.as_deref(),
                Some(origin.as_str()),
                "{origin:?}: the LEDGER row carries the typed origin"
            );
            assert!(
                !row.before_structure.is_empty() && !row.after_structure.is_empty(),
                "{origin:?}: before/after carry real structure hashes"
            );
            let frames = run.frame_hot_records().count();
            assert!(
                frames >= 2,
                "{origin:?}: both frames committed (hot store holds {frames})"
            );

            // 4. Events folded: the act's session events landed in the run
            // (persistence cursor advanced past zero).
            let cursor = run.event_cursor(&format!("persistence:{}", sess.id));
            assert!(
                cursor.is_some() && cursor.unwrap() > 0,
                "{origin:?}: session events were folded into the run"
            );
        })
        .await
        .unwrap_or_else(|e| panic!("{marker}: job failed: {e}"));
    }
}

/// Parity with the pipeline path: `drive_pipeline` under authorized
/// dispatch does NOT double-book the ledger — the executor's sink commit
/// IS the evidence, and the pipeline's dedup keeps one row per act.
#[tokio::test]
async fn pipeline_and_sink_commit_are_one_not_two() {
    let pool = SessionPool::new();
    let id = spawn(&pool, "DEDUP").await;
    pool.with_session(Some(&id), move |sess| {
        let run = Arc::new(Mutex::new(RunContext::ephemeral()));
        let sink = RunEvidenceSink::capture(&run);
        let ticket = sink.ticket().clone();
        sess.install_evidence_sink(sink);

        let action = key_x();
        let spec = CoreDriveSpec {
            action: &action,
            quiet_ms: 120,
            budget_ms: 1200,
            no_wait: false,
            visibility: InputVisibility::Normal,
            completion: tui_lab::capture::CompletionPolicy::StableScreen,
            guard: None,
            scenario: None,
            origin: DriveOrigin::Act,
            ticket,
        };
        let outcome = tui_lab::execution::drive_pipeline(sess, &run, spec)
            .unwrap_or_else(|e| panic!("pipeline drive failed: {e:#}"));
        sess.take_evidence_sink();

        assert!(
            outcome.health.healthy(),
            "the sink's commit health flows through: {:?}",
            outcome.health.failures()
        );
        let count = run.lock().unwrap().transactions().len();
        assert_eq!(
            count, 1,
            "authorized dispatch + pipeline must produce ONE ledger row, got {count}"
        );
    })
    .await
    .expect("dedup job");
}

/// Evidence health is observable on the sink itself: a closed run records
/// the commit failures instead of swallowing them.
#[tokio::test]
async fn sink_records_unhealthy_commits() {
    let pool = SessionPool::new();
    let id = spawn(&pool, "HEALTH").await;
    pool.with_session(Some(&id), move |sess| {
        let run = Arc::new(Mutex::new(RunContext::ephemeral()));
        let sink = RunEvidenceSink::capture(&run);
        sess.install_evidence_sink(sink);

        // Close the run: the ledger record must refuse (ensure_open), the
        // act itself still executes — the TUI got the key either way.
        run.lock().unwrap().close().expect("close");

        let tx = tui_lab::execution::execute_act_as(
            sess,
            DriveOrigin::Audit,
            &key_x(),
            120,
            1200,
            false,
        )
        .expect("the act succeeds even when the evidence cannot commit");

        // The failing legs are RECORDED, not swallowed (finding 9): the
        // sink keeps the health record naming what did not commit.
        let sink = sess.take_evidence_sink().expect("sink");
        let health = sink.last_health().expect("health recorded");
        assert!(
            !health.healthy(),
            "closed run: health says the record is incomplete"
        );
        assert!(
            !health.ledger_recorded,
            "the ledger leg is named as not recorded"
        );
        assert_eq!(
            tx.origin,
            Some(DriveOrigin::Audit),
            "the transaction itself is unchanged by the evidence failure"
        );
        let health = sink.last_health().expect("health recorded");
        assert!(
            !health.healthy(),
            "closed run: health says the record is incomplete"
        );
        assert!(
            !health.ledger_recorded,
            "the ledger leg is named as not recorded"
        );
    })
    .await
    .expect("health job");
}
