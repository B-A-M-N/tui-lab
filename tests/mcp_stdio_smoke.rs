mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_full_lifecycle() {
    let mut mcp = McpProc::spawn();

    // --- initialize handshake ---
    initialize(&mut mcp, "tui-lab-e2e");
    // --- tools/list: exactly the registry's tool surface (item 69's pin,
    // held over the wire). The count is asserted against the registry's own
    // capability table, never a hand-maintained number (audit P1-49) ---
    let tools = mcp.request("tools/list", serde_json::json!({}));
    let mut names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    let registry = mcp.tool("tui_run", serde_json::json!({ "action": "context" }));
    let mut declared: Vec<String> = registry["data"]["registry"]["tools"]
        .as_array()
        .expect("registry tools")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    declared.sort();
    assert_eq!(names, declared, "tools/list == capability registry");
    // Audit P1-49: the count pinned to the REGISTRY (TOOLS.len()), not a
    // literal — adding a tool updates the pin automatically.
    assert_eq!(
        names.len(),
        tui_lab::mcp::registry::TOOLS.len(),
        "registry count matches the wire: {names:?}"
    );
    // Audit P1.8: context serves the canonical flows over the wire, and
    // every flow step names a real tool.
    let flows = &registry["data"]["flows"];
    for required in [
        "debug_existing_tui",
        "construct_or_refine_tui",
        "regression_test_tui",
    ] {
        let flow = &flows[required];
        assert!(
            flow["steps"].as_array().is_some_and(|s| !s.is_empty()),
            "flow {required} served with steps: {flows}"
        );
        for step in flow["steps"].as_array().unwrap() {
            let tool = step["tool"].as_str().expect("step tool");
            assert!(
                names.contains(&tool.to_string()),
                "flow {required} references '{tool}' which is not a served tool"
            );
        }
    }
    for expected in [
        "tui_session",
        "tui_observe",
        "tui_act",
        "tui_wait",
        "tui_probe",
        "tui_assert",
        "tui_checkpoint",
        "tui_scenario",
        "tui_record",
        "tui_explore",
        "tui_audit",
        "tui_coverage",
        "tui_framework",
        "tui_run",
        "tui_contract",
        "tui_explain",
        "tui_intent",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected} in {names:?}"
        );
    }

    // --- start a real session ---
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('READY'); import time; time.sleep(300)"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start failed: {start}");
    let session = start["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();
    assert!(
        start["data"]["capabilities"].is_object(),
        "capabilities missing"
    );

    // --- observe: READY is on screen ---
    let obs = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );
    let text = obs["data"]["viewport_text"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.as_str()).collect::<String>())
        .unwrap_or_default();
    assert!(
        text.contains("READY"),
        "READY missing from observation: {obs}"
    );
    // --- observe mode=tree: hierarchical rendering (Wave-4) ---
    let obs_tree = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "tree", "id": session }),
    );
    assert_eq!(obs_tree["category"], "success", "tree mode: {obs_tree}");
    let rendered = obs_tree["data"]["rendered"].as_str().unwrap_or_default();
    assert!(
        rendered.contains("screen"),
        "tree render must include the root node: {obs_tree}"
    );
    assert!(
        obs_tree["data"]["tree"]["root"]["bounds"].is_object(),
        "tree carries bounds: {obs_tree}"
    );

    // --- observe mode=nodes: the Wave-C SemanticNode tree ---
    let obs_nodes = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "nodes", "id": session }),
    );
    assert_eq!(obs_nodes["category"], "success", "nodes mode: {obs_nodes}");
    let nrendered = obs_nodes["data"]["rendered"].as_str().unwrap_or_default();
    assert!(
        nrendered.contains("screen"),
        "node tree render must include the root: {obs_nodes}"
    );
    assert!(
        obs_nodes["data"]["tree"]["root"]["children"].is_array()
            || obs_nodes["data"]["tree"]["root"].get("children").is_none(),
        // A blank screen legitimately yields no children (the field is
        // skip_serializing_if empty) — the shape contract is the root node
        // itself plus layer tags.
        "node tree root is well-formed: {obs_nodes}"
    );
    assert!(
        obs_nodes["data"]["layers"].is_object(),
        "node tree carries layer tags: {obs_nodes}"
    );

    // --- act: type text + enter ---
    let act = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "type", "text": "echo e2e", "id": session }),
    );
    assert_eq!(act["category"], "success", "type failed: {act}");

    // --- probe: the Wave-2 troubleshooting primitive over the wire ---
    // A drift probe (no stimulus) must return an envelope with the exact
    // provenance string, an honest settle, and the material-change/transition shape.
    let probe = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "kind": "none" },
            "completion": "stable",
            "budget_ms": 3000,
            "id": session,
        }),
    );
    assert_eq!(probe["category"], "success", "probe failed: {probe}");
    assert_eq!(
        probe["data"]["action"], "no stimulus (drift)",
        "drift probe provenance: {probe}"
    );
    assert_eq!(probe["data"]["settle"], "Skipped", "drift settle: {probe}");
    assert!(
        probe["data"]["material_changes"].is_array() && probe["data"]["transition"].is_object(),
        "probe carries material_changes + transition: {probe}"
    );

    // A stimulated probe reports the exact canonical signature.
    let probe2 = mcp.tool(
        "tui_probe",
        serde_json::json!({
            "stimulus": { "kind": "key", "key": "x" },
            "completion": "may_be_silent",
            "budget_ms": 3000,
            "id": session,
        }),
    );
    assert_eq!(probe2["category"], "success", "probe2 failed: {probe2}");
    assert_eq!(
        probe2["data"]["action"], "x",
        "key probe provenance is the exact signature: {probe2}"
    );

    // --- wait: screen stable after the action ---
    let wait = mcp.tool(
        "tui_wait",
        serde_json::json!({ "condition": "screen_stable", "budget_ms": 3000, "id": session }),
    );
    assert_eq!(wait["category"], "success", "wait failed: {wait}");

    // --- assert: echo output appears (python echoes typed text) ---
    let asrt = mcp.tool(
        "tui_assert",
        serde_json::json!({ "assertion": "text", "text": "e2e", "id": session }),
    );
    // python3 -c with sleep echoes nothing itself; the TTY echoes input, so
    // accept pass OR fail with category assertion_failed (transport must be
    // success either way — a typo category is the contract breach).
    assert!(
        asrt["category"] == "success" || asrt["category"] == "assertion_failed",
        "assert envelope wrong: {asrt}"
    );

    // --- diff (re-review item 7): previous→current through ONE canonical path ---
    // First observation: no prior frame — honest null, never a self→self diff.
    let obs1 = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "diff", "id": session }),
    );
    assert_eq!(obs1["category"], "success", "diff 1: {obs1}");
    // Earlier observes already established a prior frame, and nothing changed
    // since: a real previous→current diff with no movement.
    assert_eq!(
        obs1["data"]["since"], "previous_observation",
        "diff 1: {obs1}"
    );
    assert_eq!(
        obs1["data"]["transition"]["before_structure_hash"],
        obs1["data"]["transition"]["after_structure_hash"],
        "unchanged frames must diff equal: {obs1}"
    );
    // Real transition visible through mode=diff: the act executor's own
    // post-action observation consumes the transition into `last`, so the
    // change must come from the child ASYNC (script prints on its own
    // schedule between two observations).
    let start_c = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "print('FIRST'); import time; time.sleep(3); print('DELTA-7'); time.sleep(60)"] }),
    );
    assert_eq!(start_c["category"], "success", "session C start: {start_c}");
    let sess_c = start_c["data"]["session"]
        .as_str()
        .expect("session c")
        .to_string();
    // Baseline observation (FIRST on screen).
    let obs_c0 = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": sess_c }),
    );
    assert_eq!(obs_c0["category"], "success", "obs C0: {obs_c0}");
    // Wait for the async DELTA-7 to land, by text (robust: the change may
    // land before the wait's baseline is captured, in which case a
    // change-since-now wait would never fire).
    let wc = mcp.tool(
        "tui_wait",
        serde_json::json!({ "condition": "text", "text": "DELTA-7", "budget_ms": 15000, "id": sess_c }),
    );
    assert_eq!(wc["category"], "success", "text wait: {wc}");
    assert_eq!(wc["data"]["met"], true, "DELTA-7 must appear: {wc}");
    // Now diff: previous observation (pre-DELTA) → current (DELTA-7).
    let obs2 = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "diff", "id": sess_c }),
    );
    assert_eq!(obs2["category"], "success", "diff 2: {obs2}");
    assert_eq!(obs2["data"]["since"], "previous_observation");
    let tr = &obs2["data"]["transition"];
    assert!(tr.is_object(), "canonical Transition missing: {obs2}");
    // The canonical Transition shape (same as InteractionTransaction).
    assert!(tr["screen_diff"].is_object(), "screen_diff missing: {obs2}");
    assert!(
        tr["semantic_diff"].is_object(),
        "semantic_diff missing: {obs2}"
    );
    assert_ne!(
        tr["before_structure_hash"], tr["after_structure_hash"],
        "async DELTA-7 changed the screen; hashes must differ: {tr}"
    );
    let added = serde_json::to_string(&tr["screen_diff"]["text_added"]).unwrap_or_default();
    let all = format!("{added}{}", tr["screen_diff"]);
    assert!(
        all.contains("DELTA-7"),
        "screen_diff must surface the async DELTA-7 change: {tr}"
    );
    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_c }),
    );

    // --- checkpoint save + compare ---
    let cp = mcp.tool(
        "tui_checkpoint",
        serde_json::json!({ "action": "save", "name": "e2e-cp", "id": session }),
    );
    assert_eq!(cp["category"], "success", "checkpoint save failed: {cp}");
    assert_eq!(cp["data"]["name"], "e2e-cp");
    let cp_compare = mcp.tool(
        "tui_checkpoint",
        serde_json::json!({ "action": "compare", "name": "e2e-cp", "id": session }),
    );
    assert_eq!(
        cp_compare["category"], "success",
        "compare failed: {cp_compare}"
    );

    // --- exploration (re-review items 12/13): live transitions + real budget
    // Explore a session that reacts to keys so transitions actually occur.
    let start_e = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "print('MENU'); import sys, time; i=0;\nwhile True:\n c=sys.stdin.read(1);\n i+=1; print(f'KEY{i}') if c else None; time.sleep(0.01)"] }),
    );
    // This python may die on EOF stdin reads — a dead process is also a valid
    // exploration target (relaunch budget). Either way the tool must succeed.
    let start_e_cat = start_e["category"].clone();
    if start_e_cat == "success" {
        let sess_e = start_e["data"]["session"].as_str().unwrap().to_string();
        let ex = mcp.tool(
            "tui_explore",
            serde_json::json!({ "mode": "random", "seed": 7, "actions": 5, "id": sess_e }),
        );
        assert_eq!(ex["category"], "success", "explore: {ex}");
        let rep = &ex["data"]["report"];
        // Ordered step record present (item 12).
        let steps = rep["steps"].as_array().expect("steps array");
        assert_eq!(
            steps.len() as u64,
            rep["actions_run"].as_u64().unwrap_or(99)
        );
        for (i, s) in steps.iter().enumerate() {
            assert_eq!(s["seq"], i as u64, "steps must be ordered: {s}");
            assert!(s["action"].is_string(), "step action: {s}");
            // Layered identity (re-review P0 fix 3): before/after carry
            // content/visual/interaction, not a bare structure hash.
            assert!(
                s["before"]["content"].is_string()
                    && s["before"]["interaction"].is_string()
                    && s["after"]["content"].is_string()
                    && s["after"]["visual"].is_string(),
                "step identity: {s}"
            );
            assert!(
                s["settle"].is_string(),
                "step settle status (met/timed_out/skipped): {s}"
            );
        }
        // Real completion reason, never a generic "completed" (item 13).
        let reason = rep["completion_reason"].as_str().expect("reason");
        assert!(
            [
                "action_budget",
                "time_budget",
                "relaunch_budget",
                "depth_budget",
                "unique_state_budget",
                "clean_exit",
                "failure",
                "cancelled"
            ]
            .contains(&reason),
            "unknown completion reason: {reason}"
        );
        // 5 actions requested, budget default max_actions=50 → action_budget.
        assert_eq!(reason, "action_budget", "5 actions < budget: {rep}");
        // The run's state graph must hold the executed transitions with REAL
        // action names (tab/down/...) — not synthetic "step-N" edges.
        let graph = mcp.tool("tui_explore", serde_json::json!({ "mode": "state_graph" }));
        assert_eq!(graph["category"], "success", "state_graph: {graph}");
        let edges = graph["data"]["edges"].as_array().expect("edges");
        assert!(
            !edges.is_empty(),
            "explorer must record transitions into the run graph: {graph}"
        );
        let actions_seen: Vec<&str> = edges.iter().filter_map(|e| e[2].as_str()).collect();
        assert!(
            actions_seen.iter().all(|a| !a.starts_with("step-")),
            "no synthetic step-N edges allowed: {actions_seen:?}"
        );
        assert!(
            actions_seen.iter().any(|a| [
                "tab",
                "down",
                "up",
                "enter",
                "escape",
                "left",
                "right",
                "pageup",
                "pagedown",
                "home",
                "end",
                "space",
                "shift+tab"
            ]
            .contains(a)),
            "edges must carry real action names: {actions_seen:?}"
        );
        let _ = mcp.tool(
            "tui_session",
            serde_json::json!({ "action": "stop", "id": sess_e }),
        );
    }

    // --- audit: static profile through stdio ---
    let audit = mcp.tool(
        "tui_audit",
        serde_json::json!({ "profile": "focus", "id": session }),
    );
    assert_eq!(audit["category"], "success", "audit failed: {audit}");
    assert!(audit["data"]["findings"].is_array());

    // --- stop the smoke target and terminate the real MCP server. ---
    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
    mcp.terminate();
}
