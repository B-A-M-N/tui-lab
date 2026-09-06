//! Beta-audit P0-4: lifecycle authorization for session stop/restart.
//!
//! The adversarial cases the audit demanded, on the real server surface:
//! - foreign + leased stop must refuse (the lease is read even though the
//!   session belongs to a different run)
//! - closed-run + leased stop must refuse (the lease is read even though
//!   the run gate would normally mask it)
//! - expired-lease stop succeeds
//! - same-run unleased cleanup after close succeeds
//! - foreign unleased stop refuses
//!
//! The core regression: the old handler read the lease through with_sess,
//! whose closed-run/foreign guards run BEFORE the lease read and whose Err
//! the handler discarded — so a leased foreign session could be killed.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

fn params_typed<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}

fn unwrap_ok(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = raw
        .structured_content
        .clone()
        .or_else(|| {
            raw.content.first().map(|c| {
                let rmcp::model::ContentBlock::Text(t) = c else {
                    panic!("{}: non-text content block", ctx);
                };
                serde_json::from_str(&t.text).expect("valid JSON envelope")
            })
        })
        .expect("structured content");
    assert!(!raw.is_error.unwrap_or(false), "{} failed: {}", ctx, v);
    v.get("data").cloned().expect("data payload")
}

fn unwrap_err(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    assert_eq!(raw.is_error, Some(true), "{} must fail", ctx);
    raw.structured_content.clone().expect("error envelope")
}

/// The server's session-pool handle, for liveness checks and cleanup
/// kills that deliberately bypass lifecycle authorization.
struct PoolGuard(std::sync::Arc<tui_lab::session::SessionPool>);
impl PoolGuard {
    fn new(server: &tui_lab::mcp::tools::TuiLabServer) -> Self {
        PoolGuard(server.pool_handle())
    }
}
impl std::ops::Deref for PoolGuard {
    type Target = tui_lab::session::SessionPool;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

async fn start_session(server: &tui_lab::mcp::tools::TuiLabServer, script: &str) -> String {
    let raw = server
        .tui_session(params_typed(serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", script],
            "cols": 80, "rows": 24,
        })))
        .await;
    unwrap_ok(&raw, "session start")["session"]
        .as_str()
        .expect("session id")
        .to_string()
}

async fn lease(server: &tui_lab::mcp::tools::TuiLabServer, id: &str, ttl_ms: u64) {
    let take = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "holder": "human", "ttl_ms": ttl_ms,
            })))
            .await,
        "lease take",
    );
    assert_eq!(take["leased"], true, "{take}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_leased_stop_refuses() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let guard = PoolGuard::new(&server);
    let id = start_session(&server, "print('fl'); import sys; sys.stdin.read(1)").await;
    lease(&server, &id, 60_000).await;
    // Run switch: the session is now foreign to the CURRENT run.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "run new",
    );
    // The killer combination: with_sess refuses foreign sessions BEFORE
    // reading the lease; the old handler discarded that refusal and
    // stopped the process anyway. Stop must refuse on the LEASE.
    let stop = server
        .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": id })))
        .await;
    let e = unwrap_err(&stop, "foreign+leased stop");
    assert_eq!(e["category"], "control_leased", "{e:?}");
    assert_eq!(e["details"]["holder"], "human", "{e:?}");
    // And the process survives.
    assert!(
        guard.list().contains(&id),
        "the leased foreign session must still be alive"
    );
    // Cleanup (the session genuinely is foreign now).
    guard.stop(&id).await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_run_leased_stop_refuses() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let guard = PoolGuard::new(&server);
    let id = start_session(&server, "print('cl'); import sys; sys.stdin.read(1)").await;
    lease(&server, &id, 60_000).await;
    // Close the run: with_sess would refuse on RunClosed before any lease
    // read — the old handler discarded that too.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "close" })))
            .await,
        "close",
    );
    let stop = server
        .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": id })))
        .await;
    let e = unwrap_err(&stop, "closed+leased stop");
    assert_eq!(e["category"], "control_leased", "{e:?}");
    assert!(
        guard.list().contains(&id),
        "the leased session must survive a closed run's stop attempt"
    );
    guard.stop(&id).await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_lease_allows_stop() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let guard = PoolGuard::new(&server);
    let id = start_session(&server, "print('exp'); import sys; sys.stdin.read(1)").await;
    // Short TTL; wait it out (generously — actor dispatch latency must
    // not flake this).
    lease(&server, &id, 150).await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let stop = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": id })))
            .await,
        "stop after expiry",
    );
    assert_eq!(stop["stopped"], id.as_str(), "{stop}");
    assert!(!guard.list().contains(&id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_run_unleased_cleanup_after_close_succeeds() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let guard = PoolGuard::new(&server);
    let id = start_session(&server, "print('cu'); import sys; sys.stdin.read(1)").await;
    // Close WITHOUT kill_sessions: the owned session survives, still
    // owned by the (now closed) run.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "close" })))
            .await,
        "close",
    );
    // Same-run cleanup after close is legal — no lease, session owned by
    // the current (closed) run.
    let stop = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": id })))
            .await,
        "same-run unleased stop after close",
    );
    assert_eq!(stop["stopped"], id.as_str(), "{stop}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_unleased_stop_refuses() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let guard = PoolGuard::new(&server);
    let id = start_session(&server, "print('fu'); import sys; sys.stdin.read(1)").await;
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "run new",
    );
    // No lease this time: the refusal is OWNERSHIP — the current run must
    // not manage another run's session lifecycle.
    let stop = server
        .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": id })))
        .await;
    let e = unwrap_err(&stop, "foreign unleased stop");
    assert_eq!(e["category"], "no_session", "{e:?}");
    assert!(
        e["error"]
            .as_str()
            .unwrap_or_default()
            .contains("bound to run"),
        "the refusal names the owning run: {e:?}"
    );
    assert!(
        guard.list().contains(&id),
        "the foreign session must still be alive"
    );
    guard.stop(&id).await.expect("cleanup");
}
