mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_exploration_safety_safe_never_clicks() {
    let mut mcp = McpProc::spawn();
    initialize(&mut mcp, "tui-lab-e2e-explore");
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start", "command": "python3",
            "args": ["fixtures/click_trap_tui.py"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "fixture start: {start}");
    let session = start["data"]["session"].as_str().unwrap().to_string();

    // Sanity: the trap is armed — the screen shows DETONATE, not ACTIVATED.
    let pre = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );
    let pre_text = pre["data"]["viewport_text"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.as_str()).collect::<String>())
        .unwrap_or_default();
    assert!(
        pre_text.contains("DETONATE") && !pre_text.contains("ACTIVATED"),
        "trap must start armed: {pre_text:?}"
    );

    // The typo'd risk spelling must be refused, not silently widened to
    // mutating (audit P0-5).
    let typo = mcp.tool(
        "tui_explore",
        serde_json::json!({ "mode": "semantic", "max_risk": "saef", "actions": 3, "id": session }),
    );
    assert_eq!(
        typo["category"], "invalid_request",
        "typo'd max_risk must be invalid_request: {typo}"
    );

    // The safe-only semantic exploration: whatever it does, the button must
    // not fire.
    let ex = mcp.tool(
        "tui_explore",
        serde_json::json!({ "mode": "semantic", "max_risk": "safe", "actions": 6, "id": session }),
    );
    assert_eq!(ex["category"], "success", "safe explore: {ex}");

    // Post-check: the trap must still be armed. Any step that had clicked
    // the button would have flipped the screen to ACTIVATED.
    let post = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );
    let post_text = post["data"]["viewport_text"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.as_str()).collect::<String>())
        .unwrap_or_default();
    assert!(
        !post_text.contains("ACTIVATED"),
        "max_risk=safe exploration must NEVER activate the button: {post_text:?}"
    );

    // Cross-check the honesty of the record: any focus_target step the
    // explorer DID propose is risk-classed mutating in the report (or was
    // filtered out entirely) — the old Safe label must not reappear.
    let steps = ex["data"]["report"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for s in steps.iter() {
        let motive = s["motive"].as_str().unwrap_or("");
        if motive.contains("click-focus") {
            panic!("a click-focus candidate executed under max_risk=safe: {s}");
        }
    }

    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop: {stop}");
}
