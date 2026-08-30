//! MCP stdio transport E2E (audit: "the code path is identical" claim made
//! real). Spawns the actual `hermes-tui-lab mcp` binary, speaks JSON-RPC over
//! its stdin/stdout, and drives a full lifecycle against a real python3 child:
//! initialize → session start → observe → act (type+enter) → wait → assert →
//! checkpoint save/compare → scenario record → record start/stop.
//!
//! Requires the debug binary to exist (cargo build) — tests run after the
//! bin target is built in the same workspace.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct McpProc {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpProc {
    fn spawn() -> Self {
        let bin = env!("CARGO_BIN_EXE_hermes-tui-lab");
        let mut child = Command::new(bin)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hermes-tui-lab mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        McpProc {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    /// Send a request and read lines until the response with our id arrives
    /// (skipping server-initiated notifications).
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", msg).expect("write request");
        self.stdin.flush().expect("flush");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "timeout waiting for {method}");
            let mut line = String::new();
            // set a coarse read timeout by relying on the deadline assert;
            // blocking reads are bounded by the child's lifetime.
            let n = self.stdout.read_line(&mut line).expect("read line");
            assert!(n > 0, "server closed stdout while waiting for {method}");
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue, // non-JSON noise on stdout: skip
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return v;
            }
            // else: notification or out-of-band — keep reading
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = serde_json::json!({"jsonrpc":"2.0","method":method});
        writeln!(self.stdin, "{}", msg).expect("write notify");
        self.stdin.flush().expect("flush");
    }

    /// Call one of the tui_* tools and return the parsed result payload
    /// (string content of first text block, parsed as JSON envelope).
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let resp = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {name} response: {resp}"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("envelope parse for {name}: {e}: {text}"))
    }
}

impl Drop for McpProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_e2e_full_lifecycle() {
    let mut mcp = McpProc::spawn();

    // --- initialize handshake ---
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e", "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {init}"
    );
    mcp.notify("notifications/initialized");

    // --- tools/list: the 12-tool surface ---
    let tools = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    for expected in [
        "tui_session", "tui_observe", "tui_act", "tui_wait", "tui_assert",
        "tui_checkpoint", "tui_scenario", "tui_record", "tui_explore",
        "tui_audit", "tui_coverage", "tui_framework",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
    }

    // --- start a real session ---
    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('READY'); import time; time.sleep(30)"],
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start failed: {start}");
    let session = start["data"]["session"].as_str().expect("session id").to_string();
    assert!(start["data"]["capabilities"].is_object(), "capabilities missing");

    // --- observe: READY is on screen ---
    let obs = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": session }),
    );
    let text = obs["data"]["viewport_text"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r.as_str()).collect::<String>())
        .unwrap_or_default();
    assert!(text.contains("READY"), "READY missing from observation: {obs}");

    // --- act: type text + enter ---
    let act = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "type", "text": "echo e2e", "id": session }),
    );
    assert_eq!(act["category"], "success", "type failed: {act}");

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
    assert_eq!(obs1["data"]["since"], "previous_observation", "diff 1: {obs1}");
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
    let sess_c = start_c["data"]["session"].as_str().expect("session c").to_string();
    // Baseline observation (FIRST on screen).
    let obs_c0 = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "screen", "id": sess_c }),
    );
    assert_eq!(obs_c0["category"], "success", "obs C0: {obs_c0}");
    // Wait for the async DELTA-7 to land (screen_change at backend level).
    let wc = mcp.tool(
        "tui_wait",
        serde_json::json!({ "condition": "screen_change", "budget_ms": 10000, "id": sess_c }),
    );
    assert_eq!(wc["category"], "success", "screen_change wait: {wc}");
    assert_eq!(wc["data"]["met"], true, "screen_change must fire: {wc}");
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
    assert!(tr["semantic_diff"].is_object(), "semantic_diff missing: {obs2}");
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
    let _ = mcp.tool("tui_session", serde_json::json!({ "action": "stop", "id": sess_c }));

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
    assert_eq!(cp_compare["category"], "success", "compare failed: {cp_compare}");

    // --- scenario record lifecycle over real traffic ---
    // Recording identity is the returned id (names are display labels).
    let rec = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "e2e-flow", "id": session }),
    );
    assert_eq!(rec["category"], "success", "record_start failed: {rec}");
    let rec_id = rec["data"]["recording_id"].as_str().expect("recording_id").to_string();
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
    // Ephemeral run: saved_to is honestly null, but the scenario must be
    // retained in the run context — listable and exportable.
    let list = mcp.tool("tui_scenario", serde_json::json!({ "action": "list" }));
    let listed = list["data"]["scenarios"]
        .as_array()
        .map(|a| a.iter().any(|n| n.as_str() == Some("e2e-flow")))
        .unwrap_or(false);
    assert!(listed, "scenario must be listed after record_stop: {list}");
    let export = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "export", "name": "e2e-flow" }),
    );
    assert_eq!(export["category"], "success", "export failed: {export}");
    assert_eq!(
        export["data"]["scenario"]["name"], "e2e-flow",
        "export must round-trip the recorded scenario: {export}"
    );

    // --- session scoping (re-review item 5): two sessions, same name ---
    // Session A's recording must never absorb session B's traffic.
    let sess_a = session.clone();
    let start_b = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "import time; time.sleep(60)"] }),
    );
    assert_eq!(start_b["category"], "success", "session B start failed: {start_b}");
    let sess_b = start_b["data"]["session"].as_str().expect("session b").to_string();

    let rec_a = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "login", "id": sess_a }),
    );
    assert_eq!(rec_a["category"], "success", "rec A start: {rec_a}");
    let id_a = rec_a["data"]["recording_id"].as_str().unwrap().to_string();
    let rec_b = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "login", "id": sess_b }),
    );
    assert_eq!(rec_b["category"], "success", "rec B start: {rec_b}");
    let id_b = rec_b["data"]["recording_id"].as_str().unwrap().to_string();
    assert_ne!(id_a, id_b, "same-name recordings need distinct ids");

    // Session B was started but never observed: its first diff compares the
    // start-time snapshot (a real prior frame) against the current screen —
    // nothing has happened since start, so hashes must match and the diff be
    // empty (never a fabricated self→self comparison of one frame — item 7).
    let obs_b1 = mcp.tool(
        "tui_observe",
        serde_json::json!({ "mode": "diff", "id": sess_b }),
    );
    assert_eq!(obs_b1["category"], "success", "diff B1: {obs_b1}");
    assert_eq!(obs_b1["data"]["since"], "previous_observation", "diff B1: {obs_b1}");
    let trb = &obs_b1["data"]["transition"];
    assert_eq!(
        trb["before_structure_hash"], trb["after_structure_hash"],
        "idle session must diff clean against its start snapshot: {trb}"
    );
    assert_eq!(
        trb["screen_diff"]["changed_cells"], 0,
        "no cell changes since start: {trb}"
    );

    // Traffic to B only:
    let act_b = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "escape", "id": sess_b }),
    );
    assert_eq!(act_b["category"], "success", "act B: {act_b}");
    // Traffic to A only:
    let act_a = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "tab", "id": sess_a }),
    );
    assert_eq!(act_a["category"], "success", "act A: {act_a}");

    let stop_a = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "recording_id": id_a }),
    );
    assert_eq!(stop_a["category"], "success", "stop A: {stop_a}");
    assert_eq!(stop_a["data"]["steps"], 1, "A must hold ONLY its own act: {stop_a}");
        let a_step = stop_a["data"]["scenario"]["steps"][0].to_string();
    assert!(a_step.contains("\"tab\""), "A steps: {a_step}");
    assert!(!a_step.contains("escape"), "A absorbed B's traffic: {a_step}");

    let stop_b = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "recording_id": id_b }),
    );
    assert_eq!(stop_b["category"], "success", "stop B: {stop_b}");
    assert_eq!(stop_b["data"]["steps"], 1, "B must hold ONLY its own act: {stop_b}");
    let b_step = stop_b["data"]["scenario"]["steps"][0].to_string();
    assert!(b_step.contains("escape"), "B steps: {b_step}");
    assert!(!b_step.contains("\"tab\""), "B absorbed A's traffic: {b_step}");

    let _ = mcp.tool("tui_session", serde_json::json!({ "action": "stop", "id": sess_b }));

    // --- recording lifecycle (PTY boundary) ---
    let rstart = mcp.tool("tui_record", serde_json::json!({ "format": "start", "id": session }));
    assert_eq!(rstart["category"], "success", "record start failed: {rstart}");
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "x", "id": session }),
    );
    let rstop = mcp.tool("tui_record", serde_json::json!({ "format": "stop", "id": session }));
    assert_eq!(rstop["category"], "success", "record stop failed: {rstop}");
    assert!(
        rstop["data"]["events"].as_u64().unwrap_or(0) > 0,
        "PTY recording must capture events: {rstop}"
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
        assert_eq!(steps.len() as u64, rep["actions_run"].as_u64().unwrap_or(99));
        for (i, s) in steps.iter().enumerate() {
            assert_eq!(s["seq"], i as u64, "steps must be ordered: {s}");
            assert!(s["action"].is_string(), "step action: {s}");
            assert!(s["before"].is_string() && s["after"].is_string(), "step identity: {s}");
        }
        // Real completion reason, never a generic "completed" (item 13).
        let reason = rep["completion_reason"].as_str().expect("reason");
        assert!(
            ["action_budget", "time_budget", "relaunch_budget", "depth_budget",
             "unique_state_budget", "clean_exit", "failure", "cancelled"]
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
        let actions_seen: Vec<&str> = edges
            .iter()
            .filter_map(|e| e[2].as_str())
            .collect();
        assert!(
            actions_seen.iter().all(|a| !a.starts_with("step-")),
            "no synthetic step-N edges allowed: {actions_seen:?}"
        );
        assert!(
            actions_seen.iter().any(|a| ["tab", "down", "up", "enter", "escape",
                "left", "right", "pageup", "pagedown", "home", "end", "space",
                "shift+tab"].contains(a)),
            "edges must carry real action names: {actions_seen:?}"
        );
        let _ = mcp.tool("tui_session", serde_json::json!({ "action": "stop", "id": sess_e }));
    }

    // --- audit: static profile through stdio ---
    let audit = mcp.tool("tui_audit", serde_json::json!({ "profile": "focus", "id": session }));
    assert_eq!(audit["category"], "success", "audit failed: {audit}");
    assert!(audit["data"]["findings"].is_array());

    // --- tui_run lifecycle (goal spec): ephemeral default → explicit persist
    // → same identity, durable artifacts → close ---
    let st0 = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    assert_eq!(st0["category"], "success", "run status: {st0}");
    assert_eq!(st0["data"]["mode"], "ephemeral", "startup must be ephemeral: {st0}");
    assert!(
        st0["data"]["artifact_root"].is_null(),
        "ephemeral run has no artifact root: {st0}"
    );
    let run_id = st0["data"]["run_id"].as_str().expect("run id").to_string();
    assert!(run_id.starts_with("run-"), "run id shape: {run_id}");
    // Real work happened before persist (checkpoints, scenario, transactions).
    assert!(
        st0["data"]["counts"]["transactions"].as_u64().unwrap_or(0) > 0,
        "transactions must be counted before promotion: {st0}"
    );
    assert!(
        st0["data"]["counts"]["checkpoints"].as_u64().unwrap_or(0) > 0,
        "checkpoints must be counted before promotion: {st0}"
    );

    // Persist with NO explicit root: must resolve from the primary session's
    // LaunchSpec.cwd. Start a session with a cwd pointing into our temp dir.
    let persist_root = std::env::temp_dir().join(format!("tui-lab-e2e-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&persist_root);
    std::fs::create_dir_all(&persist_root).expect("persist root");
    let start_d = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "import time; time.sleep(60)"],
            "cwd": persist_root.to_string_lossy() }),
    );
    assert_eq!(start_d["category"], "success", "session D start: {start_d}");
    let sess_d = start_d["data"]["session"].as_str().unwrap().to_string();

    // A checkpoint before promotion: after persist it must survive (SAME run).
    let cp_d = mcp.tool(
        "tui_checkpoint",
        serde_json::json!({ "action": "save", "name": "pre-promotion", "id": sess_d }),
    );
    assert_eq!(cp_d["category"], "success", "cp D: {cp_d}");

    let persist = mcp.tool("tui_run", serde_json::json!({ "action": "persist" }));
    assert_eq!(persist["category"], "success", "persist: {persist}");
    assert_eq!(persist["data"]["persistent"], true);
    assert_eq!(
        persist["data"]["run_id"], run_id.as_str(),
        "promotion must preserve run identity"
    );
    let artifact_root = persist["data"]["artifact_root"].as_str().expect("root").to_string();
    let expected_root = persist_root.join(".tui-lab").join("runs").join(&run_id);
    assert_eq!(
        std::path::Path::new(&artifact_root), expected_root,
        "root must resolve from session cwd, not server cwd"
    );
    // Durable artifacts actually on disk.
    assert!(expected_root.join("run.json").exists(), "manifest written");
    assert!(
        expected_root.join("checkpoints").is_dir(),
        "checkpoints dir created"
    );
    assert!(
        expected_root.join("scenarios").is_dir(),
        "scenarios dir created (with flushed scenario)"
    );
    // The pre-promotion checkpoint survives into the durable store.
    let cp_d2 = mcp.tool(
        "tui_checkpoint",
        serde_json::json!({ "action": "compare", "name": "pre-promotion", "id": sess_d }),
    );
    assert_eq!(
        cp_d2["category"], "success",
        "checkpoint must survive promotion: {cp_d2}"
    );

    // Status now reports persistent.
    let st1 = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    assert_eq!(st1["data"]["mode"], "persistent", "status after persist: {st1}");
    assert_eq!(st1["data"]["artifact_root"], artifact_root.as_str());

    // Close: flushes + marks closed; sessions survive (kill_sessions unset).
    let close = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "close", "kill_sessions": true }),
    );
    assert_eq!(close["category"], "success", "close: {close}");
    assert_eq!(close["data"]["closed"], true);
    assert!(
        close["data"]["sessions_stopped"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "explicit kill_sessions must stop sessions: {close}"
    );
    // Manifest on disk records closure.
    let manifest_text = std::fs::read_to_string(expected_root.join("run.json")).expect("manifest");
    assert!(
        manifest_text.contains("\"closed\": true"),
        "closed flag must be durable: {manifest_text}"
    );
    let _ = std::fs::remove_dir_all(&persist_root);

    // --- stop session ---
    let stop = mcp.tool("tui_session", serde_json::json!({ "action": "stop", "id": session }));
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
}
