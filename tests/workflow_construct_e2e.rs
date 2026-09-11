//! Beta-audit P1.1: `tui_workflow action=construct` — the task-oriented
//! entry point for greenfield/understanding work.
//!
//! The guarantee: ONE safe call joins what would otherwise be a six-tool
//! chain — live session capabilities, the SAME inspection view
//! `tui_observe mode=inspect` serves, project/framework identity, native
//! adapter status, the loaded contract, candidate invariants (status
//! `unverified`, never promoted), and exact next invocations. It never
//! drives the app (no key is ever sent) and never edits source.

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

fn workflow_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiWorkflowParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn construct_joins_the_chain_in_one_safe_call() {
    let server = TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-p11c-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // Drive one construct with NO session and an explicit cwd: the
    // framework join must answer from that tree (or honestly unknown)
    // — never the server's own directory.
    let no_session = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "construct", "cwd": base.to_string_lossy(),
            })))
            .await,
    );
    assert_eq!(no_session["category"], "success", "{no_session}");
    let no_session = &no_session["data"];
    assert!(
        no_session["session"].is_null(),
        "no session selected — construct works from a bare run: {no_session}"
    );
    assert_eq!(no_session["contract"]["loaded"], false, "{no_session}");
    let candidates = no_session["candidate_invariants"]
        .as_array()
        .expect("candidates");
    assert!(
        !candidates.is_empty() && candidates.iter().all(|c| c["status"] == "unverified"),
        "every candidate is unverified — never promoted to a declared fact: {no_session}"
    );
    let next = no_session["next_invocations"]
        .as_array()
        .expect("next invocations");
    assert!(
        next.iter().any(|n| n["call"] == "tui_observe")
            && next.iter().any(|n| n["call"] == "tui_contract"),
        "construct names the exact next calls: {no_session}"
    );

    // With a live session: capabilities + the shared inspection view
    // ride the packet.
    let start = result_json(
        &server
            .tui_session(session_params(json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('== P11 =='); print('[ Save ]  [ Quit ]'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
    );
    assert_eq!(start["category"], "success", "{start}");
    let sid = start["data"]["session"].as_str().unwrap().to_string();

    let construct = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "construct", "id": sid, "cwd": base.to_string_lossy(),
            })))
            .await,
    );
    assert_eq!(construct["category"], "success", "{construct}");
    let data = &construct["data"];
    assert_eq!(data["session"]["session"], sid, "{construct}");
    assert!(
        data["session"]["capabilities"]["mouse"].is_boolean(),
        "live capabilities ride the packet: {construct}"
    );
    let inspection = &data["session"]["inspection"];
    assert_eq!(
        inspection["viewport"]["cols"], 80,
        "the SAME inspection view tui_observe serves: {construct}"
    );
    assert!(
        inspection["controls"].is_array(),
        "per-control facts ride along: {construct}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

/// construct NEVER drives: the app must receive no input during the
/// call. A counter child proves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn construct_never_drives_the_app() {
    let server = TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-p11d-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");
    let counter = base.join("keys");
    std::fs::write(&counter, "0").expect("seed");
    let script = format!(
        r#"
import sys, tty
tty.setraw(0)
path = {counter:?}
count = 0
print("READY")
sys.stdout.flush()
while True:
    ch = sys.stdin.buffer.read(1)
    if ch and ch != b'\x00':
        count += 1
        open(path, "w").write(str(count))
"#
    );
    let start = result_json(
        &server
            .tui_session(session_params(json!({
                "action": "start", "command": "python3",
                "args": ["-c", script], "cols": 80, "rows": 24,
            })))
            .await,
    );
    let sid = start["data"]["session"].as_str().unwrap().to_string();

    for _ in 0..2 {
        let out = result_json(
            &server
                .tui_workflow(workflow_params(json!({
                    "action": "construct", "id": sid,
                })))
                .await,
        );
        assert_eq!(out["category"], "success", "{out}");
    }
    let sent: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("value");
    assert_eq!(sent, 0, "construct must not send a single key");

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}
