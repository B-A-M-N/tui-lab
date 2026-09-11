mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_closed_run_refuses_driving() {
    let mut mcp = McpProc::spawn();
    initialize(&mut mcp, "tui-lab-e2e");
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('closed-run-ready'); input()"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start: {start}");
    let session = start["data"]["session"].as_str().unwrap().to_string();

    // Baseline traffic succeeds while the run is open.
    let act0 = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "enter", "id": session }),
    );
    assert_eq!(act0["category"], "success", "act while open: {act0}");
    let tx_before = mcp.tool("tui_run", serde_json::json!({ "action": "status" }))["data"]
        ["counts"]["transactions"]
        .as_u64()
        .expect("tx count");

    // Close WITHOUT killing the session: the review's exact hazard shape —
    // the session stays live and resolvable after the run is sealed.
    let close = mcp.tool("tui_run", serde_json::json!({ "action": "close" }));
    assert_eq!(close["category"], "success", "close: {close}");
    assert_eq!(close["data"]["closed"], true);
    assert!(
        close["data"]["sessions_stopped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "close must not stop sessions by default: {close}"
    );

    // Every driving tool now refuses with run_closed.
    for (name, args) in [
        (
            "tui_act",
            serde_json::json!({ "action": "key", "key": "enter", "id": session }),
        ),
        (
            "tui_wait",
            serde_json::json!({ "condition": "screen_stable", "budget_ms": 100, "id": session }),
        ),
        (
            "tui_assert",
            serde_json::json!({ "assertion": "text", "text": "x", "id": session }),
        ),
        (
            "tui_probe",
            serde_json::json!({ "stimulus": { "kind": "none" }, "completion": "stable", "budget_ms": 100, "id": session }),
        ),
    ] {
        let mut a = args;
        a["id"] = serde_json::json!(session);
        let resp = mcp.tool(name, a);
        assert_eq!(
            resp["category"], "run_closed",
            "{name} must refuse on a closed run: {resp}"
        );
        assert!(
            resp["error"]
                .as_str()
                .map(|m| m.contains("closed") && m.contains("resume"))
                .unwrap_or(false),
            "error names the state and the remedy: {resp}"
        );
    }

    // The closed run absorbed none of it.
    let st = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    assert_eq!(st["data"]["closed"], true, "still closed: {st}");
    assert_eq!(
        st["data"]["counts"]["transactions"].as_u64().expect("tx"),
        tx_before,
        "transaction count frozen at close"
    );

    // Read surfaces stay available on a closed run (list over a real dir —
    // a missing runs dir is an empty list, a clean response, not an error).
    let empty_root = std::env::temp_dir().join(format!("tui-lab-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty_root);
    std::fs::create_dir_all(&empty_root).expect("empty root");
    let list = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "list", "root": empty_root.to_string_lossy() }),
    );
    assert_eq!(list["category"], "success", "list on closed run: {list}");
    let _ = std::fs::remove_dir_all(&empty_root);

    // Session cleanup (the pool still works; sessions were never killed).
    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop: {stop}");
}
