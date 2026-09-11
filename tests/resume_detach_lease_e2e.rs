//! Beta-audit P0.2: resume-detach vs. the human control lease.
//!
//! The defect: `resume(detach_existing_sessions=true)` called the raw
//! session stop path for foreign sessions, bypassing the lifecycle
//! authorization every other stop/restart route honors — a run resume
//! could kill a session a human was actively driving.
//!
//! The fix under test: a live human lease REFUSES the whole resume
//! before anything is disturbed (the caller explicitly asked for
//! detach, so silent skip would promise less than was asked); the
//! resume transaction also commits the run transition BEFORE the
//! destructive detach, and a detach failure after commit is reported
//! as partial — never faked as a rollback.

use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;
use tui_lab::mcp::TuiLabServer;

fn raw_ready(marker: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("import sys,tty; tty.setraw(0); print('{marker}'); sys.stdin.buffer.read(1)"),
    ]
}

fn result_text(out: &rmcp::model::CallToolResult) -> String {
    out.content
        .first()
        .and_then(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn result_json(out: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&result_text(out)).unwrap_or(serde_json::Value::Null)
}

fn run_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiRunParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn session_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiSessionParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

/// THE defect scenario: a human holds a lease on a foreign session;
/// `resume(detach_existing_sessions=true)` must refuse the whole
/// resume naming the leased session — and the session must STILL BE
/// ALIVE. Before the fix, the detach stopped it without checking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_detach_refuses_over_a_live_human_lease() {
    let s = TuiLabServer::new();
    // Two sessions: one plain foreign, one leased by a human.
    async fn start(s: &TuiLabServer, name: &str) -> String {
        let out = s
            .tui_session(session_params(json!({
                "action": "start", "command": "python3",
                "args": raw_ready(name), "cols": 80, "rows": 24,
            })))
            .await;
        result_json(&out)["data"]["session"]
            .as_str()
            .expect("session id")
            .to_string()
    }
    let plain = start(&s, "PLAIN").await;
    let leased = start(&s, "LEASED").await;
    let take = s
        .tui_session(session_params(json!({
            "action": "lease", "id": leased, "holder": "human", "ttl_ms": 60_000,
        })))
        .await;
    assert_eq!(result_json(&take)["data"]["leased"], json!(true));

    // Build a resumable target with the sessions FOREIGN to it: swap
    // to a fresh run B (the sessions stay owned by A), persist B, then
    // resume B — the sessions are then foreign to the target.
    let swap = s
        .tui_run(run_params(json!({ "action": "new", "discard": true })))
        .await;
    assert!(
        !swap.is_error.unwrap_or(false),
        "swap failed: {}",
        result_text(&swap)
    );
    let persist = s
        .tui_run(run_params(json!({
            "action": "persist",
            "root": std::env::temp_dir().join("tui-lab-resume-lease-test"),
        })))
        .await;
    assert!(
        !persist.is_error.unwrap_or(false),
        "persist failed: {}",
        result_text(&persist)
    );
    let resumed_dir = result_json(&persist)["data"]["artifact_root"]
        .as_str()
        .expect("artifact root")
        .to_string();

    // resume with detach: must REFUSE on the leased session, BEFORE
    // touching anything.
    let out = s
        .tui_run(run_params(json!({
            "action": "resume", "run_dir": resumed_dir, "detach_existing_sessions": true,
        })))
        .await;
    let text = result_text(&out);
    assert!(
        text.contains("leased"),
        "resume detach must refuse over a live human lease: {text}"
    );
    let body = result_json(&out);
    assert_eq!(body["category"], json!("control_leased"), "{body}");
    assert_eq!(
        body["details"]["session"],
        json!(leased),
        "the refusal must name the leased session: {body}"
    );
    assert_eq!(body["details"]["resume"], json!("not_started"));

    // BOTH sessions must still be alive — the refusal is pre-transaction.
    for sid in [&plain, &leased] {
        let live = s.pool_handle().list().iter().any(|x| x.as_str() == sid);
        assert!(live, "session {sid} must survive the refused resume");
    }
    // The lease obviously still holds — the refusal named it. (A
    // status readback through with_sess now refuses the FOREIGN
    // session, which is itself correct post-swap behavior.)
    // Cleanup: release the lease, then stop both (authorized now).
    let lease_id = result_json(&take)["data"]["lease_id"]
        .as_str()
        .expect("lease_id")
        .to_string();
    let _ = s
        .tui_session(session_params(json!({
            "action": "release", "id": leased, "lease_id": lease_id,
        })))
        .await;
    s.pool_handle().stop(&plain).await.ok();
    s.pool_handle().stop(&leased).await.ok();
}

/// The transaction order: a resume that will fail its target preflight
/// must not stop any foreign session even when detach was requested.
/// (The destructive detach runs only after the transition commits.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_never_runs_when_the_resume_fails_before_commit() {
    let s = TuiLabServer::new();
    let out = s
        .tui_session(session_params(json!({
            "action": "start", "command": "python3",
            "args": raw_ready("DETACH ORDER"), "cols": 80, "rows": 24,
        })))
        .await;
    let sid = result_json(&out)["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();
    // No leased target exists: resume cannot restore — detach must not
    // have run even though it was requested.
    let out = s
        .tui_run(run_params(json!({
            "action": "resume",
            "run_dir": "/nonexistent/resume-order-check",
            "detach_existing_sessions": true,
        })))
        .await;
    assert!(result_text(&out).contains("cannot restore"));
    let live = s.pool_handle().list().iter().any(|x| x.as_str() == sid);
    assert!(
        live,
        "a failed resume must never destroy sessions — detach is post-commit cleanup"
    );
    s.pool_handle().stop(&sid).await.ok();
}

/// resume WITHOUT detach and with foreign sessions refuses naming
/// them; nothing changes (this path predates P0.2 but the refusal
/// ordering is part of the same transaction contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_resume_refuses_on_foreign_sessions_without_detach() {
    let s = TuiLabServer::new();
    let out = s
        .tui_session(session_params(json!({
            "action": "start", "command": "python3",
            "args": raw_ready("NO DETACH"), "cols": 80, "rows": 24,
        })))
        .await;
    let sid = result_json(&out)["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();
    // Swap to run B and persist it; the session stays owned by run A,
    // so resuming B makes the session foreign to the target.
    let _ = s
        .tui_run(run_params(json!({ "action": "new", "discard": true })))
        .await;
    let persist = s
        .tui_run(run_params(json!({
            "action": "persist",
            "root": std::env::temp_dir().join("tui-lab-resume-nodetach-test"),
        })))
        .await;
    assert!(!persist.is_error.unwrap_or(false));
    let dir = result_json(&persist)["data"]["artifact_root"]
        .as_str()
        .unwrap()
        .to_string();
    let out = s
        .tui_run(run_params(json!({ "action": "resume", "run_dir": dir })))
        .await;
    let text = result_text(&out);
    assert!(
        text.contains("do not belong to target run") && text.contains("detach_existing_sessions"),
        "plain resume must refuse on foreign sessions: {text}"
    );
    let live = s.pool_handle().list().iter().any(|x| x.as_str() == sid);
    assert!(live, "refusal leaves sessions alone");
    s.pool_handle().stop(&sid).await.ok();
}
