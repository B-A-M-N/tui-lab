mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn resources_list_and_read_live_state() {
    let mut mcp = McpProc::spawn();
    initialize(&mut mcp, "tui-lab-e2e");
    // A live session so the session resources resolve.
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('res-ready'); input()"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start: {start}");
    let sid = start["data"]["session"].as_str().expect("sid").to_string();
    let run_id = start["data"]["run"].as_str().expect("run id").to_string();

    // resources/templates/list: the declared templates.
    let templates = mcp.request("resources/templates/list", serde_json::json!({}));
    let tlist: Vec<String> = templates["result"]["resourceTemplates"]
        .as_array()
        .expect("templates array")
        .iter()
        .filter_map(|t| t["uriTemplate"].as_str().map(str::to_string))
        .collect();
    assert!(
        tlist.contains(&"tui://sessions/{session_id}/semantic".to_string()),
        "semantic template declared: {tlist:?}"
    );
    assert!(
        tlist.contains(&"tui://runs/{run_id}".to_string()),
        "run template declared: {tlist:?}"
    );

    // resources/list: concrete resources include the findings ledger.
    let list = mcp.request("resources/list", serde_json::json!({}));
    let names: Vec<String> = list["result"]["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .filter_map(|r| r["uri"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|u| u == "tui://findings"),
        "findings resource listed: {names:?}"
    );

    // read the semantic view: valid JSON naming the on-screen marker.
    let sem = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://sessions/{sid}/semantic") }),
    );
    assert!(sem["result"]["contents"].is_array(), "semantic read: {sem}");
    let text = sem["result"]["contents"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("controls"), "semantic payload: {text}");
    assert!(
        sem["result"]["contents"][0]["uri"]
            .as_str()
            .unwrap_or("")
            .ends_with("/semantic"),
        "content carries its uri: {sem}"
    );

    // read the screen view: viewport text carries the child's output.
    // The child boots (and prints) asynchronously — under parallel test
    // load the first read can precede its output, so poll until the marker
    // shows or the budget burns.
    let mut stext = String::new();
    for _ in 0..40 {
        let screen = mcp.request(
            "resources/read",
            serde_json::json!({ "uri": format!("tui://sessions/{sid}/screen") }),
        );
        stext = screen["result"]["contents"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if stext.contains("res-ready") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(stext.contains("res-ready"), "screen payload: {stext}");

    // read the run manifest: id matches the session-start echo.
    let run = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}") }),
    );
    let rtext = run["result"]["contents"][0]["text"].as_str().unwrap_or("");
    assert!(rtext.contains(&run_id), "run manifest: {rtext}");

    // findings ledger reads (empty is fine; the envelope must be valid).
    let find = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": "tui://findings" }),
    );
    let ftext = find["result"]["contents"][0]["text"].as_str().unwrap_or("");
    assert!(ftext.contains("\"findings\""), "findings payload: {ftext}");

    // --- Review P1: evidence-addressable resources. ---

    // A finding id that does not exist: not_found naming how many ARE
    // available (self-correction payload, not a bare 404).
    let badf = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": "tui://findings/absent@00000000" }),
    );
    assert_eq!(badf["error"]["code"], -32002, "bad finding: {badf}");

    // The transaction ledger collection for the live run.
    let txs = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/transactions") }),
    );
    let txtext = txs["result"]["contents"][0]["text"].as_str().unwrap_or("");
    assert!(txtext.contains("\"retained\""), "ledger listing: {txtext}");
    // At least one transaction exists? Drive one to be sure, then re-read.
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "x", "id": sid }),
    );
    let txs2 = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/transactions") }),
    );
    let txv: serde_json::Value =
        serde_json::from_str(txs2["result"]["contents"][0]["text"].as_str().unwrap_or(""))
            .expect("ledger JSON");
    assert!(
        txv["retained"].as_u64().unwrap_or(0) >= 1,
        "one act → at least one ledger record: {txv}"
    );
    // One transaction by seq: the first record's fields are legible.
    let seq0 = txv["transactions"][0]["seq"].as_u64().expect("ledger seq");
    let tx1 = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/transactions/{seq0}") }),
    );
    let t1v: serde_json::Value =
        serde_json::from_str(tx1["result"]["contents"][0]["text"].as_str().unwrap_or(""))
            .expect("transaction JSON");
    assert_eq!(t1v["seq"], seq0, "single transaction read: {t1v}");
    assert!(t1v["action"].is_string(), "{t1v}");
    // A wrong seq is an honest not_found with the retained window size.
    let badseq = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/transactions/99999") }),
    );
    assert_eq!(badseq["error"]["code"], -32002, "bad seq: {badseq}");
    let badmsg = badseq["error"]["message"].as_str().unwrap_or("");
    assert!(
        badmsg.contains("retained window"),
        "not-found names the window: {badmsg}"
    );

    // The scenario collection (empty at this point, but shape must hold).
    let scn = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/scenarios") }),
    );
    let sv: serde_json::Value =
        serde_json::from_str(scn["result"]["contents"][0]["text"].as_str().unwrap_or(""))
            .expect("scenarios JSON");
    assert_eq!(sv["count"], 0, "no scenarios yet: {sv}");

    // Record + stop a scenario, then address it as a resource.
    let rec = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "res-scen", "id": sid }),
    );
    assert_eq!(rec["category"], "success", "record: {rec}");
    let rec_id = rec["data"]["recording_id"].as_str().expect("recording id");
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "tab", "id": sid }),
    );
    let stop = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "recording_id": rec_id, "id": sid }),
    );
    assert_eq!(stop["category"], "success", "record_stop: {stop}");
    let scen_id = stop["data"]["scenario_id"].as_str().expect("scenario id");
    let scn2 = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/scenarios") }),
    );
    let sv2: serde_json::Value =
        serde_json::from_str(scn2["result"]["contents"][0]["text"].as_str().unwrap_or(""))
            .expect("scenarios JSON");
    assert_eq!(sv2["count"], 1, "one scenario: {sv2}");
    assert_eq!(
        sv2["scenarios"][0]["id"], scen_id,
        "collection carries the id and its uri: {sv2}"
    );
    // One scenario by id → the full step list.
    let one = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/scenarios/{scen_id}") }),
    );
    let ov: serde_json::Value =
        serde_json::from_str(one["result"]["contents"][0]["text"].as_str().unwrap_or(""))
            .expect("scenario JSON");
    assert_eq!(ov["id"], scen_id, "{ov}");
    assert!(!ov["steps"].as_array().expect("steps").is_empty(), "{ov}");
    // Unknown scenario key → not_found from load_scenario's own resolution.
    let badscn = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_id}/scenarios/absent") }),
    );
    assert_eq!(badscn["error"]["code"], -32002, "bad scenario: {badscn}");

    // unknown session id → protocol-level resource_not_found (code -32002),
    // NOT a success envelope with empty content.
    let bad = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://sessions/{sid}-nope/screen") }),
    );
    assert_eq!(bad["error"]["code"], -32002, "unknown session: {bad}");

    // --- Wave G items 74+75: persist this run, close it, then read the
    // CLOSED run through tui://runs/<id> from a SECOND server (the browser
    // reads disk; a fresh server's live run differs, so the read takes the
    // on-disk path and marks live=false). ---
    let browser_base =
        std::env::temp_dir().join(format!("tui-lab-e2e-browser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&browser_base);
    std::fs::create_dir_all(&browser_base).expect("browser base");
    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sid }),
    );
    let start2 = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start", "command": "python3",
            "args": ["-c", "print('browser-ready'); input()"],
            "cwd": browser_base.to_string_lossy(),
        }),
    );
    assert_eq!(start2["category"], "success", "browser session: {start2}");
    let run_closed = start2["data"]["run"].as_str().unwrap().to_string();
    let sess_closed = start2["data"]["session"].as_str().unwrap().to_string();
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "tab", "id": sess_closed }),
    );
    let persist2 = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "persist", "root": browser_base.to_string_lossy() }),
    );
    assert_eq!(persist2["category"], "success", "persist2: {persist2}");
    let close2 = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "close", "kill_sessions": true }),
    );
    assert_eq!(close2["category"], "success", "close2: {close2}");

    // A FRESH server (different live run) reads the closed run from disk.
    // Spawned inside browser_base: cwd is the workspace anchor that lets a
    // restarted server find runs persisted there.
    let mut mcp2 = McpProc::spawn_in(&browser_base);
    let init2 = mcp2.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e-2", "version": "0" },
        }),
    );
    assert!(init2["result"]["serverInfo"]["name"].is_string(), "init2");
    mcp2.notify("notifications/initialized");
    let closed_read = mcp2.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_closed}") }),
    );
    let ctext = closed_read["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or("");
    let cv: serde_json::Value =
        serde_json::from_str(ctext).expect("closed-run resource payload is JSON");
    assert_eq!(cv["run_id"], run_closed.as_str(), "{cv}");
    assert_eq!(cv["closed"], true, "{cv}");
    assert_eq!(
        cv["live"], false,
        "browser read marks the run not-live: {cv}"
    );
    assert!(
        cv["counts"]["transactions"].as_u64().unwrap_or(0) >= 1,
        "ledger count restored in the browser payload: {cv}"
    );

    // tui_run action=list from the second server sees it, with the count.
    let list2 = mcp2.tool(
        "tui_run",
        serde_json::json!({ "action": "list", "root": browser_base.to_string_lossy() }),
    );
    assert_eq!(list2["category"], "success", "list2: {list2}");
    let listed = list2["data"]["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["run_id"].as_str() == Some(run_closed.as_str()))
        .expect("closed run listed")
        .clone();
    assert_eq!(listed["closed"], true, "{listed}");
    assert!(
        listed["ledger_transactions"].as_u64().unwrap_or(0) >= 1,
        "ledger count listed: {listed}"
    );

    // Resume it in the second server; the resource read now takes the live
    // branch (no browser marker).
    let resume = mcp2.tool(
        "tui_run",
        serde_json::json!({
            "action": "resume", "run_id": run_closed,
            "root": browser_base.to_string_lossy(),
        }),
    );
    assert_eq!(resume["category"], "success", "resume: {resume}");
    let live_read = mcp2.request(
        "resources/read",
        serde_json::json!({ "uri": format!("tui://runs/{run_closed}") }),
    );
    let ltext = live_read["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or("");
    let lv: serde_json::Value =
        serde_json::from_str(ltext).expect("live-run resource payload is JSON");
    assert!(
        lv.get("live").is_none(),
        "live run read does not carry the browser marker: {lv}"
    );
    assert_eq!(lv["run_id"], run_closed.as_str(), "{lv}");
    let _ = std::fs::remove_dir_all(&browser_base);

    // unknown scheme → resource_not_found naming the accepted templates.
    let bad2 = mcp.request(
        "resources/read",
        serde_json::json!({ "uri": "tui://bogus/thing" }),
    );
    assert_eq!(bad2["error"]["code"], -32002, "bogus scheme: {bad2}");

    mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sid }),
    );
}
