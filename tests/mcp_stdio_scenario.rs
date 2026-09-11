//! Scenario recording and replay through the real MCP stdio transport.

mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_scenario_record_and_replay() {
    let mut mcp = McpProc::spawn();
    initialize(&mut mcp, "tui-lab-e2e-scenario");

    // A stable session hosts the scenario lifecycle.
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('scenario-ready'); import time; time.sleep(300)"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start failed: {start}");
    let session = start["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();

    // --- scenario record lifecycle over real traffic ---
    // Recording identity is the returned id (names are display labels).
    let rec = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "e2e-flow", "id": session }),
    );
    assert_eq!(rec["category"], "success", "record_start failed: {rec}");
    let rec_id = rec["data"]["recording_id"]
        .as_str()
        .expect("recording_id")
        .to_string();
    assert!(
        rec_id.starts_with("rec-"),
        "recording id must be rec-<uuid>: {rec}"
    );
    assert_eq!(rec["data"]["generation"], 1);
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "tab", "id": session }),
    );
    // record_stop by NAME must still work (resolves oldest active by name).
    let stop = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "name": "e2e-flow" }),
    );
    assert_eq!(stop["category"], "success", "record_stop failed: {stop}");
    assert_eq!(stop["data"]["steps"], 1, "recorded steps: {stop}");
    assert_eq!(stop["data"]["recording_id"], rec_id.as_str());
    // Saved scenarios have true ids (Wave-1 item 9): record_stop returns one
    // and the list is keyed by id, not name.
    let scen_id = stop["data"]["scenario_id"]
        .as_str()
        .expect("scenario_id")
        .to_string();
    assert!(scen_id.starts_with("scn-"), "scenario id shape: {stop}");
    // Ephemeral run: saved_to is honestly null, but the scenario must be
    // retained in the run context — listable and exportable.
    let list = mcp.tool("tui_scenario", serde_json::json!({ "action": "list" }));
    let listed = list["data"]["scenarios"]
        .as_array()
        .map(|a| a.iter().any(|n| n.as_str() == Some(scen_id.as_str())))
        .unwrap_or(false);
    assert!(
        listed,
        "scenario must be listed by id after record_stop: {list}"
    );
    let export = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "export", "name": "e2e-flow" }),
    );
    assert_eq!(export["category"], "success", "export failed: {export}");
    assert_eq!(
        export["data"]["scenario"]["name"], "e2e-flow",
        "export must round-trip the recorded scenario: {export}"
    );

    // --- recording lifecycle (PTY boundary) ---
    let rstart = mcp.tool(
        "tui_record",
        serde_json::json!({ "format": "start", "id": session }),
    );
    assert_eq!(
        rstart["category"], "success",
        "record start failed: {rstart}"
    );
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "x", "id": session }),
    );
    let rstop = mcp.tool(
        "tui_record",
        serde_json::json!({ "format": "stop", "id": session }),
    );
    assert_eq!(rstop["category"], "success", "record stop failed: {rstop}");
    assert!(
        rstop["data"]["events"].as_u64().unwrap_or(0) > 0,
        "PTY recording must capture events: {rstop}"
    );
    assert_eq!(
        rstop["data"]["held_in_run"], true,
        "ephemeral stop must hold the recording: {rstop}"
    );

    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
    mcp.terminate();
}
