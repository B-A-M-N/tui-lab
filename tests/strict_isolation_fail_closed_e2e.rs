//! Beta-audit P0.8: `isolation=strict` fails CLOSED.
//!
//! The defect: strict advertised "no network" (README) but if `unshare`
//! was missing or `unshare --net` was not permitted, the launch
//! proceeded anyway — networked — and merely REPORTED the failure
//! afterward. Observability is not enforcement.
//!
//! The invariant under test, at the MCP surface: a strict start either
//! returns a session whose IsolationEvidence reads `verified` (the net
//! namespace was actually proven by preflighting the exact operation)
//! or refuses the launch outright. There is no outcome in which a
//! session runs under strict without proven isolation.

use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;
use tui_lab::mcp::TuiLabServer;

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

fn session_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiSessionParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

/// The full disjunction, driven through the public tool surface. Either
/// the launch is REFUSED (error envelope, no session), or it succeeded
/// AND the evidence says the namespace was proven (`verified`). A
/// session with `failed` / `unverified` / `not_applied` under strict is
/// the old fail-open bug and fails this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_start_is_verified_or_refused_never_networked() {
    let server = TuiLabServer::new();
    let out = server
        .tui_session(session_params(json!({
            "action": "start", "command": "python3",
            "args": ["-c", "print('READY'); input()"],
            "cols": 80, "rows": 24,
            "isolation": "strict",
        })))
        .await;
    let body = result_json(&out);

    if out.is_error.unwrap_or(false) {
        // The fail-closed refusal: names the proof failure and the
        // explicit clean downgrade.
        let text = result_text(&out);
        assert!(
            text.contains("cannot be proven") || text.contains("REFUSED"),
            "refusal names the proof failure: {text}"
        );
        assert!(
            text.contains("isolation=clean"),
            "refusal names the deliberate downgrade: {text}"
        );
        assert!(
            body["data"]["session"].is_null(),
            "a refused strict launch must not return a session: {body}"
        );
    } else {
        // Proven path: the evidence must read verified — not applied-
        // anyway-with-a-story. This is the branch hosts with working
        // net-ns take.
        let evidence = &body["data"]["isolation"];
        assert_eq!(
            evidence["profile"], "strict",
            "strict evidence rides the launch: {body}"
        );
        assert_eq!(
            evidence["network_isolated"], "verified",
            "a strict session runs ONLY under proven isolation: {body}"
        );
        let _ = server
            .tui_session(session_params(json!({
                "action": "stop", "id": body["data"]["session"]
            })))
            .await;
    }
}

/// `clean` on the same host is unchanged: it is the explicit, visible
/// downgrade the strict refusal points at, and it launches with
/// `not_applied` (no namespace) — honestly reported, never claimed.
/// Clean isolation scrubs PATH, so the interpreter is named by absolute
/// path (the harness's own contract: clean = no environment trust).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_remains_the_explicit_downgrade() {
    let python3: String = {
        let out = std::process::Command::new("which")
            .arg("python3")
            .output()
            .expect("which python3");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert!(
        python3.starts_with('/'),
        "python3 must resolve for clean isolation: {python3:?}"
    );
    let server = TuiLabServer::new();
    let body = result_json(
        &server
            .tui_session(session_params(json!({
                "action": "start", "command": python3,
                "args": ["-c", "print('READY'); input()"],
                "cols": 80, "rows": 24,
                "isolation": "clean",
            })))
            .await,
    );
    assert_eq!(body["category"], "success", "{body}");
    let evidence = &body["data"]["isolation"];
    assert_eq!(evidence["profile"], "clean", "{body}");
    // VerifiedState serializes in its variant spelling on the wire.
    assert_eq!(
        evidence["network_isolated"], "NotApplied",
        "clean does not claim network isolation: {body}"
    );
    let _ = server
        .tui_session(session_params(json!({
            "action": "stop", "id": body["data"]["session"]
        })))
        .await;
}
