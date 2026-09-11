mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_probe_lease_sensitivity_and_capture() {
    let mut mcp = McpProc::spawn();
    initialize(&mut mcp, "tui-lab-e2e-probe");
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start", "command": "python3",
            "args": ["fixtures/probe_tui.py"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start: {start}");
    let session = start["data"]["session"].as_str().unwrap().to_string();
    let _ = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );

    // 1) LEASE: a stimulated probe drives — under a human lease it must be
    // refused with control_leased, exactly like tui_act; a drift probe
    // stays observational and allowed.
    let lease = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "lease", "id": session, "by": "probe-e2e" }),
    );
    assert_eq!(lease["category"], "success", "lease: {lease}");
    // Finding 4: the release token rides the grant.
    let lease_id = lease["data"]["lease_id"]
        .as_str()
        .expect("lease_id")
        .to_string();

    let drift = mcp.tool(
        "tui_probe",
        serde_json::json!({ "stimulus": { "kind": "none" }, "completion": "stable", "budget_ms": 1500, "id": session }),
    );
    assert_eq!(
        drift["category"], "success",
        "drift probe stays allowed under lease: {drift}"
    );

    let stimulated = mcp.tool(
        "tui_probe",
        serde_json::json!({ "stimulus": { "kind": "key", "key": "x" }, "completion": "may_be_silent", "budget_ms": 1500, "id": session }),
    );
    assert_eq!(
        stimulated["category"], "control_leased",
        "stimulated probe must be refused under the lease: {stimulated}"
    );

    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "release", "id": session, "lease_id": lease_id }),
    );

    // 2) SENSITIVE stimulus: the typed secret must never surface in the
    // probe's own response evidence (the payload is redacted the same way
    // tui_act redacts it) — check the full response envelope.
    const SECRET: &str = "probe-hunter2-e2e";
    let sensitive = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "action": "type", "text": SECRET, "sensitive": true },
            "completion": "may_be_silent",
            "budget_ms": 2000,
            "id": session,
        }),
    );
    let resp_text = serde_json::to_string(&sensitive).unwrap_or_default();
    assert_eq!(
        sensitive["category"], "success",
        "sensitive probe: {sensitive}"
    );
    assert!(
        !resp_text.contains(SECRET),
        "the secret must not surface in the probe response: {resp_text}"
    );

    // 3) Stimulus provenance: the probe's action field names the canonical
    // signature, and the transition block carries before/after hashes.
    assert!(
        sensitive["data"]["before"]["structure_hash"].is_string()
            && sensitive["data"]["after"]["structure_hash"].is_string()
            && sensitive["data"]["transition"].is_object(),
        "transition carries before/after: {sensitive}"
    );

    // 4) Capture spec: frames=2 asks for post-stimulus transition frames;
    // the response reports what was captured (the count is honest, 0 is
    // legitimate for an app that did not redraw).
    let captured = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "kind": "key", "key": "y" },
            "completion": "may_be_silent",
            "budget_ms": 2000,
            "capture": { "strategy": "frames", "count": 2 },
            "id": session,
        }),
    );
    assert_eq!(captured["category"], "success", "capture probe: {captured}");
    assert!(
        captured["data"]["frames_captured"].is_u64(),
        "frames_captured reported: {captured}"
    );
    assert!(
        captured["data"]["capture"].is_object(),
        "the transition capture block rides on the response: {captured}"
    );

    // 5) Budget is a true total deadline: a probe with a tiny budget and a
    // completion that can never settle must return within a bounded factor
    // of the budget, not hang.
    let t0 = std::time::Instant::now();
    let budgeted = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "kind": "key", "key": "z" },
            "completion": "text_appears",
            "text": "THIS-NEVER-APPEARS-E2E",
            "budget_ms": 700,
            "id": session,
        }),
    );
    let elapsed = t0.elapsed();
    assert_eq!(budgeted["category"], "success", "budget probe: {budgeted}");
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "probe must honor its total budget (took {elapsed:?})"
    );

    // 6) Stimulus provenance: a stimulated probe enters the run's
    // transaction ledger (finding 56/57 parity with tui_act evidence).
    let status_before = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    let tx_before = status_before["data"]["counts"]["transactions"]
        .as_u64()
        .unwrap_or(0);
    let _ = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "kind": "key", "key": "q" },
            "completion": "may_be_silent",
            "budget_ms": 1500,
            "id": session,
        }),
    );
    let status_after = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    let tx_after = status_after["data"]["counts"]["transactions"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        tx_after > tx_before,
        "stimulated probe must enter the run ledger: {tx_before} -> {tx_after}"
    );

    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
}
