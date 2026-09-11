mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_native_cooperation_over_the_wire() {
    let mut mcp = McpProc::spawn();

    initialize(&mut mcp, "tui-lab-e2e-native");
    // Launch the shipped cooperative fixture through the server. The
    // server's cwd is the repo root (McpProc::spawn uses "."), so the
    // relative fixture path resolves exactly as an agent would name it.
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["fixtures/nsp_tui.py"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(
        start["category"], "success",
        "fixture start failed: {start}"
    );
    let session = start["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();

    // Drive focus Right (Save → Cancel) with a settle, then observe.
    let act = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "right", "id": session }),
    );
    assert_eq!(act["category"], "success", "key right failed: {act}");

    // mode=nodes: the native block must report an ACTIVE channel with the
    // app's declared framework/app names, the matched #save/#cancel ids,
    // and zero invalid frames.
    let obs = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "nodes", "id": session }),
    );
    assert_eq!(obs["category"], "success", "nodes observe failed: {obs}");
    let native = &obs["data"]["native"];
    assert!(
        native["active"].as_bool().unwrap_or(false),
        "the fixture declared frames — the channel must be active: {native}"
    );
    let st = &native["adapter_status"];
    assert_eq!(
        st["adapter_available"],
        serde_json::json!(true),
        "harness injected TUI_LAB_SEMANTIC: {st}"
    );
    assert_eq!(
        st["native_channel_active"],
        serde_json::json!(true),
        "frames landed: {st}"
    );
    assert_eq!(st["healthy"], serde_json::json!(true), "{st}");
    assert!(
        st["frames_received"].as_u64().unwrap_or(0) > 0,
        "real frames counted: {st}"
    );
    assert_eq!(st["frames_invalid"], serde_json::json!(0), "{st}");
    assert_eq!(
        native["framework"],
        serde_json::json!("raw-ansi"),
        "the app's declared framework must ride the response: {native}"
    );
    assert_eq!(
        native["app"],
        serde_json::json!("nsp-demo"),
        "the app's declared name must ride the response: {native}"
    );
    let matched = native["matched"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        matched.contains(&"#save") && matched.contains(&"#cancel"),
        "both declared buttons must join the inferred tree: {matched:?}"
    );

    // The fused focus comes from the app's declaration (focus moved right
    // → #cancel), with the native-focus evidence marker — proof the
    // overlay merged, not that a style heuristic guessed.
    let sem = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "semantic", "id": session }),
    );
    assert_eq!(sem["category"], "success", "semantic observe failed: {sem}");
    let focus = &sem["data"]["semantic"]["focus"];
    assert_eq!(
        focus["confidence"],
        serde_json::json!(1.0),
        "native focus carries confidence 1.0: {focus}"
    );
    let evidence = focus["evidence"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        evidence.iter().any(|e| e.starts_with("native-focus:")),
        "focus evidence must cite the native channel: {evidence:?}"
    );

    // mode=screen still works unchanged (native facts ride along, screen
    // stays the authority for text).
    let scr = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );
    assert_eq!(scr["category"], "success", "screen observe failed: {scr}");
    let text = scr["data"]["viewport_text"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.as_str()).collect::<String>())
        .unwrap_or_default();
    assert!(
        text.contains("Save") && text.contains("Cancel"),
        "the fixture's buttons must be on screen: {text:?}"
    );

    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
}
