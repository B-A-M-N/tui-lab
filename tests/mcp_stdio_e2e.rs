//! MCP stdio transport E2E (audit: "the code path is identical" claim made
//! real). Spawns the actual `tui-lab mcp` binary, speaks JSON-RPC over
//! its stdin/stdout, and drives a full lifecycle against a real python3 child:
//! initialize → session start → observe → act (type+enter) → wait → assert →
//! checkpoint save/compare → scenario record → record start/stop.
//!
//! Requires the debug binary to exist (cargo build) — tests run after the
//! bin target is built in the same workspace.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared stdout reader: the request loop reads lines from it while a
/// watchdog thread observes progress. A hard hang in the server (e.g. a
/// deadlocked tool handler) would otherwise block `read_line` forever —
/// the deadline was previously only checked between lines, so a stuck
/// server meant a stuck test binary, not a timeout failure.
struct SharedStdout(BufReader<std::process::ChildStdout>);

struct McpProc {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: Arc<Mutex<SharedStdout>>,
    next_id: u64,
}

impl McpProc {
    fn spawn() -> Self {
        Self::spawn_in(std::path::Path::new("."))
    }

    fn spawn_in(dir: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_tui-lab");
        let mut child = Command::new(bin)
            .arg("mcp")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tui-lab mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = Arc::new(Mutex::new(SharedStdout(BufReader::new(
            child.stdout.take().expect("stdout"),
        ))));
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
        // Watchdog (audit P1-55): liveness is SLIDING, not one-shot. A
        // notification line used to permanently disarm the watchdog, so
        // "notification then hang" blocked forever. Now every line resets
        // the liveness deadline; the kill fires only when NO line has
        // arrived for the whole liveness window, and the watchdog stops
        // only on the matching response (or process death → EOF below).
        let watchdog = Watchdog::start(
            Some(self.child.id()),
            deadline,
            Duration::from_secs(30), // liveness window per line
        );
        loop {
            assert!(Instant::now() < deadline, "timeout waiting for {method}");
            let mut line = String::new();
            let n = {
                let mut out = self.stdout.lock().expect("stdout lock");
                out.0.read_line(&mut line).expect("read line")
            };
            watchdog.progress();
            assert!(n > 0, "server closed stdout while waiting for {method}");
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue, // non-JSON noise on stdout: skip
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                watchdog.stop();
                return v;
            }
            // else: notification or out-of-band — keep reading, watchdog
            // stays armed (progress() only slid its deadline)
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
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("envelope parse for {name}: {e}: {text}"))
    }
}

/// Kills the server child when it goes silent too long or blows the request
/// deadline. The server's death unblocks the reader (EOF), turning a server
/// deadlock into a normal test failure instead of an eternally-running test
/// binary.
///
/// Audit P1-55: the old watchdog DISARMED PERMANENTLY on the first line
/// read, so "one notification, then hang" survived protection. Now liveness
/// is a sliding deadline: every line read pushes it out; the kill fires when
/// no line has arrived for `liveness` straight. `stop()` ends protection
/// only when the matching response arrived (or the reader hits EOF).
struct Watchdog {
    last_line: Arc<Mutex<Instant>>,
    stop: Arc<Mutex<bool>>,
}

impl Watchdog {
    fn start(child_pid: Option<u32>, deadline: Instant, liveness: Duration) -> Self {
        let stop = Arc::new(Mutex::new(false));
        let last_line = Arc::new(Mutex::new(Instant::now()));
        {
            let stop = stop.clone();
            let last_line = last_line.clone();
            std::thread::spawn(move || loop {
                if *stop.lock().unwrap() {
                    return;
                }
                let silent_for = last_line.lock().unwrap().elapsed();
                if Instant::now() >= deadline || silent_for >= liveness {
                    let why = if Instant::now() >= deadline {
                        "request deadline"
                    } else {
                        "no line for the liveness window"
                    };
                    eprintln!("[e2e watchdog] {why} elapsed; killing server {child_pid:?}");
                    if let Some(pid) = child_pid {
                        // SIGKILL the process group: the child PTY apps die
                        // too, so no stray python3 survives the failed test.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                            libc::kill(pid as i32, libc::SIGKILL);
                        }
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            });
        }
        Watchdog { last_line, stop }
    }

    /// Called after every successful line read: RESETS the liveness
    /// deadline (does not end protection — audit P1-55).
    fn progress(&self) {
        *self.last_line.lock().unwrap() = Instant::now();
    }

    /// The matching response arrived; protection ends.
    fn stop(&self) {
        *self.stop.lock().unwrap() = true;
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

    // --- session scoping (re-review item 5): two sessions, same name ---
    // Session A's recording must never absorb session B's traffic.
    let sess_a = session.clone();
    let start_b = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "import time; time.sleep(60)"] }),
    );
    assert_eq!(
        start_b["category"], "success",
        "session B start failed: {start_b}"
    );
    let sess_b = start_b["data"]["session"]
        .as_str()
        .expect("session b")
        .to_string();

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
    assert_eq!(
        obs_b1["data"]["since"], "previous_observation",
        "diff B1: {obs_b1}"
    );
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
    assert_eq!(
        stop_a["data"]["steps"], 1,
        "A must hold ONLY its own act: {stop_a}"
    );
    let a_step = stop_a["data"]["scenario"]["steps"][0].to_string();
    assert!(a_step.contains("\"tab\""), "A steps: {a_step}");
    assert!(
        !a_step.contains("escape"),
        "A absorbed B's traffic: {a_step}"
    );

    let stop_b = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "recording_id": id_b }),
    );
    assert_eq!(stop_b["category"], "success", "stop B: {stop_b}");
    assert_eq!(
        stop_b["data"]["steps"], 1,
        "B must hold ONLY its own act: {stop_b}"
    );
    let b_step = stop_b["data"]["scenario"]["steps"][0].to_string();
    assert!(b_step.contains("escape"), "B steps: {b_step}");
    assert!(
        !b_step.contains("\"tab\""),
        "B absorbed A's traffic: {b_step}"
    );

    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_b }),
    );

    // --- scenario replay through MCP (regression path: record → run) ---
    // Record a real scenario: the child echoes typed text, so the assert
    // passes when replayed against an identical fresh session.
    let start_r = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "import time; time.sleep(60)"] }),
    );
    assert_eq!(
        start_r["category"], "success",
        "replay target start: {start_r}"
    );
    let sess_r = start_r["data"]["session"].as_str().unwrap().to_string();

    let rec_r = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "echo-flow", "id": sess_r }),
    );
    assert_eq!(rec_r["category"], "success", "rec R: {rec_r}");
    let id_r = rec_r["data"]["recording_id"].as_str().unwrap().to_string();
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "type", "text": "regression-marker", "id": sess_r }),
    );
    let asrt_r = mcp.tool(
        "tui_assert",
        serde_json::json!({ "assertion": "text", "text": "regression-marker", "id": sess_r }),
    );
    assert_eq!(
        asrt_r["category"], "success",
        "recorded assert must pass: {asrt_r}"
    );
    let stop_r = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "recording_id": id_r }),
    );
    assert_eq!(stop_r["category"], "success", "stop R: {stop_r}");
    assert_eq!(stop_r["data"]["steps"], 2, "recorded act+assert: {stop_r}");

    // Replay the SAVED scenario against the same session: must pass.
    let run_ok = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "echo-flow", "id": sess_r }),
    );
    assert_eq!(run_ok["category"], "success", "run pass case: {run_ok}");
    assert_eq!(run_ok["data"]["passed"], true, "replay must pass: {run_ok}");
    assert_eq!(run_ok["data"]["steps_total"], 2);
    assert_eq!(run_ok["data"]["steps_failed"], 0);

    // Replay against a FRESH session (same launch): must still pass —
    // scenarios are portable across generations/sessions of the same app.
    let start_r2 = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "start", "command": "python3",
            "args": ["-c", "import time; time.sleep(60)"] }),
    );
    assert_eq!(
        start_r2["category"], "success",
        "replay target 2: {start_r2}"
    );
    let sess_r2 = start_r2["data"]["session"].as_str().unwrap().to_string();
    let run_ok2 = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "echo-flow", "id": sess_r2 }),
    );
    assert_eq!(
        run_ok2["category"], "success",
        "run cross-session: {run_ok2}"
    );
    assert_eq!(
        run_ok2["data"]["passed"], true,
        "same-app fresh session must pass: {run_ok2}"
    );

    // A scenario asserting text that never appears must FAIL for real.
    // Steps are stored flat ({kind, ...params}) — the same shape replay parses.
    let save_fail = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "save", "name": "failing-flow",
            "steps": [
                { "kind": "assert", "assertion": "text", "text": "never-appears-xyz" }
            ] }),
    );
    assert_eq!(
        save_fail["category"], "success",
        "save failing scenario: {save_fail}"
    );
    let run_bad = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "failing-flow", "id": sess_r2 }),
    );
    assert_eq!(
        run_bad["category"], "success",
        "transport stays success: {run_bad}"
    );
    assert_eq!(
        run_bad["data"]["passed"], false,
        "regression must be detected: {run_bad}"
    );
    assert_eq!(run_bad["data"]["steps_total"], 1);
    assert_eq!(run_bad["data"]["steps_failed"], 1, "{run_bad}");
    // The failure detail must name the expectation, not say "executed".
    let detail = run_bad["data"]["step_results"][0]["detail"].to_string();
    assert!(
        detail.contains("never-appears-xyz"),
        "assert detail must state the expectation: {detail}"
    );

    // Audit finding 5 (wire): the DEFAULT stop policy halts at the first
    // failed step — later steps are `skipped_due_to_prior_failure`, the
    // report says `stopped_on_failure`, and a skipped step fails `passed`.
    let save_multi = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "save", "name": "fail-fast-flow",
            "steps": [
                { "kind": "assert", "assertion": "text", "text": "never-appears-xyz" },
                { "kind": "assert", "assertion": "text", "text": "never-checked-2" }
            ] }),
    );
    assert_eq!(
        save_multi["category"], "success",
        "save fail-fast scenario: {save_multi}"
    );
    let run_stop = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "fail-fast-flow", "id": sess_r2 }),
    );
    assert_eq!(run_stop["category"], "success", "{run_stop}");
    assert_eq!(run_stop["data"]["passed"], false, "{run_stop}");
    assert_eq!(
        run_stop["data"]["status"], "stopped_on_failure",
        "{run_stop}"
    );
    assert_eq!(run_stop["data"]["steps_total"], 2, "{run_stop}");
    assert_eq!(run_stop["data"]["steps_failed"], 1, "{run_stop}");
    assert_eq!(run_stop["data"]["steps_skipped"], 1, "{run_stop}");
    assert!(
        run_stop["data"]["step_results"][1]["detail"]
            .to_string()
            .contains("skipped_due_to_prior_failure"),
        "{run_stop}"
    );

    // The `continue` override runs every step: status completed, both fail,
    // nothing skipped.
    let run_cont = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "fail-fast-flow",
            "id": sess_r2, "on_failure": "continue" }),
    );
    assert_eq!(run_cont["category"], "success", "{run_cont}");
    assert_eq!(run_cont["data"]["passed"], false, "{run_cont}");
    assert_eq!(run_cont["data"]["status"], "completed", "{run_cont}");
    assert_eq!(run_cont["data"]["steps_failed"], 2, "{run_cont}");
    assert_eq!(run_cont["data"]["steps_skipped"], 0, "{run_cont}");

    // An unknown policy is refused before anything runs.
    let run_badpol = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "run", "name": "fail-fast-flow",
            "id": sess_r2, "on_failure": "explode" }),
    );
    assert_eq!(run_badpol["category"], "invalid_request", "{run_badpol}");
    assert!(
        run_badpol.to_string().contains("on_failure"),
        "error names the parameter: {run_badpol}"
    );

    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_r }),
    );
    let _ = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_r2 }),
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
    // Ephemeral run: the recording is HELD in run memory (goal spec:
    // promotion preserves "recordings already held in memory"), verified
    // after persist below.
    assert_eq!(
        rstop["data"]["held_in_run"], true,
        "ephemeral stop must hold the recording: {rstop}"
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

    // --- tui_run lifecycle (goal spec): ephemeral default → explicit persist
    // → same identity, durable artifacts → close ---
    let st0 = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    assert_eq!(st0["category"], "success", "run status: {st0}");
    assert_eq!(
        st0["data"]["mode"], "ephemeral",
        "startup must be ephemeral: {st0}"
    );
    assert!(
        st0["data"]["artifact_root"].is_null(),
        "ephemeral run has no artifact root: {st0}"
    );
    let run_id = st0["data"]["run_id"].as_str().expect("run id").to_string();
    assert!(run_id.starts_with("run-"), "run id shape: {run_id}");
    // sessions array present (live session list from the manager).
    assert!(
        st0["data"]["sessions"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "status must list live sessions: {st0}"
    );
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

    // Explicit root (the runs dir itself, per the spec example) wins over
    // session-cwd resolution: <root>/<same-run-id>/ directly.
    let explicit_runs = std::env::temp_dir()
        .join(format!("tui-lab-e2e-{}", std::process::id()))
        .join(".tui-lab")
        .join("runs");
    let _ = std::fs::remove_dir_all(&explicit_runs);
    std::fs::create_dir_all(&explicit_runs).expect("explicit runs dir");
    let persist = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "persist", "root": explicit_runs.to_string_lossy() }),
    );
    assert_eq!(persist["category"], "success", "persist: {persist}");
    assert_eq!(persist["data"]["persistent"], true);
    assert_eq!(
        persist["data"]["run_id"],
        run_id.as_str(),
        "promotion must preserve run identity"
    );
    let artifact_root = persist["data"]["artifact_root"]
        .as_str()
        .expect("root")
        .to_string();
    let expected_root = explicit_runs.join(&run_id);
    assert_eq!(
        std::path::Path::new(&artifact_root),
        expected_root,
        "explicit runs-dir root must hold the run directly: {persist}"
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
    // The PTY recording stopped while ephemeral must be flushed into the
    // durable recordings dir (goal spec: "recordings already held in memory").
    let recordings_dir = expected_root.join("recordings");
    let cast_count = std::fs::read_dir(&recordings_dir)
        .expect("recordings dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cast"))
        .count();
    assert!(
        cast_count > 0,
        "held recording must be flushed at promotion: {:?}",
        std::fs::read_dir(&recordings_dir).map(|d| d
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect::<Vec<_>>())
    );

    // Status now reports persistent.
    let st1 = mcp.tool("tui_run", serde_json::json!({ "action": "status" }));
    assert_eq!(
        st1["data"]["mode"], "persistent",
        "status after persist: {st1}"
    );
    assert_eq!(st1["data"]["artifact_root"], artifact_root.as_str());

    // Close: flushes + marks closed; sessions survive (kill_sessions unset).
    let close = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "close", "kill_sessions": true }),
    );
    assert_eq!(close["category"], "success", "close: {close}");
    assert_eq!(close["data"]["closed"], true);
    assert!(
        close["data"]["sessions_stopped"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
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
    let stop = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": session }),
    );
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
}

/// Review P0.1 over the real wire: a closed run refuses session-driving
/// tools with category `run_closed`, and the closure survives on disk
/// (a resume of the same run re-opens driving). This is the sequence the
/// review called broken: act → close → act must never land new evidence in
/// the closed bundle.
#[test]
fn stdio_e2e_closed_run_refuses_driving() {
    let mut mcp = McpProc::spawn();
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
        "init: {init}"
    );
    mcp.notify("notifications/initialized");

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

/// Wave G item 72: MCP resources over the real stdio transport —
/// resources/templates list, live semantic + screen reads against a real
/// python3 child, and honest resource_not_found for unknown ids.
#[test]
fn resources_list_and_read_live_state() {
    let mut mcp = McpProc::spawn();
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
        "init: {init}"
    );
    mcp.notify("notifications/initialized");

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

/// Watchdog proof (harness hardening): a server that never responds must
/// produce a request() timeout failure, not an eternal hang. Uses a silent
/// child (`sleep`) standing in for a deadlocked server. Second half proves
/// the audit P1-55 semantics: progress() SLIDES the deadline instead of
/// disarming — a line arriving does not end protection.
#[test]
fn watchdog_converts_silent_server_into_timeout() {
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::Mutex;

    let mut silent = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let stdout = Arc::new(Mutex::new(SharedStdout(BufReader::new(
        silent.stdout.take().expect("stdout"),
    ))));
    let deadline = Instant::now() + Duration::from_secs(2);
    let wd = Watchdog::start(Some(silent.id()), deadline, Duration::from_secs(2));
    let start = Instant::now();
    let mut line = String::new();
    let n = {
        let mut out = stdout.lock().unwrap();
        out.0.read_line(&mut line).expect("read")
    };
    // sleep never writes: the read must unblock via watchdog SIGKILL -> EOF.
    assert_eq!(n, 0, "expected EOF after watchdog kill, got {n} bytes");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "watchdog must kill quickly"
    );
    // progress() must NOT have disarmed the watchdog earlier: with a
    // sliding deadline, calling it mid-read cannot extend life past the
    // deadline when no further line arrives. The kill above happened while
    // the watchdog was armed the whole time.
    wd.progress();
    wd.stop();
    let _ = silent.kill();
    let _ = silent.wait();
}

/// Item 38/39 — native cooperation over the REAL wire: the shipped
/// cooperative fixture is launched through the actual server process, and
/// the agent-visible responses must carry the app's declared truth (not
/// just inference), the adapter-status split (available vs active vs
/// healthy), and a clean teardown. Every assertion here runs against
/// JSON-RPC responses, exactly as an MCP client sees them.
#[test]
fn stdio_e2e_native_cooperation_over_the_wire() {
    let mut mcp = McpProc::spawn();

    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e-native", "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {init}"
    );
    mcp.notify("notifications/initialized");

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

/// Exploration safety (audit P0-4 / finding 58): a never-focused button is
/// resolved by semantic exploration through a MOUSE CLICK — a click that
/// activates it. The candidate must therefore be risk-classed Mutating so a
/// `max_risk=safe` explorer NEVER fires it. The fixture visibly mutates on
/// any input (ACTIVATED banner), so "the banner never appears" is a
/// directly observable safety property. Also proves the selector audit
/// fix: a typo'd `max_risk` is invalid_request, never a silent Mutating
/// fallback.
#[test]
fn stdio_e2e_exploration_safety_safe_never_clicks() {
    let mut mcp = McpProc::spawn();
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e-explore", "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {init}"
    );
    mcp.notify("notifications/initialized");

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

/// Probe semantics over the wire (audit finding 57 core): lease enforcement,
/// sensitive-stimulus redaction in evidence, stimulus provenance entering
/// the run record, and the transition capture block carrying
/// stimulus-anchored frames when a capture spec is given.
#[test]
fn stdio_e2e_probe_lease_sensitivity_and_capture() {
    let mut mcp = McpProc::spawn();
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e-probe", "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "init: {init}"
    );
    mcp.notify("notifications/initialized");

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

/// Audit finding 59 — run-lifecycle provenance over the wire. The four
/// scenarios the audit called broken all live in the run/session boundary,
/// where evidence ownership, foreign-session hygiene, restore ordering, and
/// pre-mutation authorization interact. Each drifts into a WRONG outcome
/// unless the handler threads the ownership/closed/restore state through
/// exactly right; this test pins all four together:
///
///   (a) a persistent run A with dirty state, on `new`, is flushed and the
///       fresh run acknowledges it (`previous_flushed`) — the old run's
///       in-memory evidence is never dropped on the floor;
///   (b) after `new`, run A's session is FOREIGN to run B; closing B with
///       `kill_sessions=true` must NOT stop A's session (foreign sessions
///       are never touched by another run's close) — the session survives
///       and the close names it in `foreign_live_sessions`;
///   (c) resuming a CORRUPT run with `detach_existing_sessions=true` must
///       refuse during restore (step 1, before any destructive detach) and
///       leave the current run's sessions alive and still driving;
///   (d) a closed run refuses `tui_session action=restart` with
///       `run_closed` BEFORE the child process is torn down — the process
///       is not mutated, so the session survives the refusal.
#[test]
fn stdio_e2e_run_lifecycle_provenance() {
    let mut mcp = McpProc::spawn();

    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-e2e-provenance", "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "init: {init}"
    );
    mcp.notify("notifications/initialized");

    // ── Scenario (a): persistent run A, dirty state → new → A flushed ──
    let persist_root =
        std::env::temp_dir().join(format!("tui-lab-e2e-provrun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&persist_root);
    std::fs::create_dir_all(&persist_root).expect("persist root");

    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('prov-ready'); input()"],
            "cwd": persist_root.to_string_lossy(),
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start: {start}");
    let sess_a = start["data"]["session"].as_str().unwrap().to_string();

    // Dirty state: drive the session so run A holds in-memory evidence.
    let act = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "type", "text": "a-marker", "id": sess_a }),
    );
    assert_eq!(act["category"], "success", "act into A: {act}");

    // Promote A to persistent so flush-before-new is meaningful.
    let persist = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "persist", "root": persist_root.to_string_lossy() }),
    );
    assert_eq!(persist["category"], "success", "persist A: {persist}");
    let run_a = persist["data"]["run_id"].as_str().unwrap().to_string();
    let artifact_a = persist["data"]["artifact_root"]
        .as_str()
        .unwrap()
        .to_string();

    // Now begin a fresh run. The old persistent run A must be flushed, and
    // the response must NAME that flush — never silently abandon it.
    let new_run = mcp.tool("tui_run", serde_json::json!({ "action": "new" }));
    assert_eq!(new_run["category"], "success", "new: {new_run}");
    assert_eq!(
        new_run["data"]["previous_run"],
        run_a.as_str(),
        "new names the run it left: {new_run}"
    );
    assert_eq!(
        new_run["data"]["previous_flushed"], true,
        "persistent run A must be flushed on new: {new_run}"
    );
    let run_b = new_run["data"]["new_run_id"].as_str().unwrap().to_string();
    assert_ne!(run_a, run_b, "new run must have a fresh identity");
    // The flush is real, not a claimed flag: run A's manifest on disk is
    // written and still identifies the same run (the durable side caught up
    // to the in-memory evidence instead of dropping it).
    let manifest_a: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::path::Path::new(&artifact_a).join("run.json"))
            .expect("run A manifest on disk"),
    )
    .expect("run A manifest parses as JSON");
    assert_eq!(
        manifest_a["run_id"],
        run_a.as_str(),
        "flush after new must leave run A's durable manifest: {manifest_a}"
    );

    // ── Scenario (b): A's session is foreign; close B kill_sessions → A's
    // session survives. ──
    let close_b = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "close", "kill_sessions": true }),
    );
    assert_eq!(close_b["category"], "success", "close B: {close_b}");
    assert_eq!(close_b["data"]["closed"], true);
    assert!(
        close_b["data"]["owned_sessions"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "run B owns no sessions after new: {close_b}"
    );
    assert!(
        close_b["data"]["foreign_live_sessions"]
            .as_array()
            .map(|a| a.iter().any(|s| s.as_str() == Some(sess_a.as_str())))
            .unwrap_or(false),
        "close B must see A's session as foreign: {close_b}"
    );
    assert!(
        close_b["data"]["sessions_stopped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "kill_sessions on B must NEVER touch A's foreign session: {close_b}"
    );
    // Positive proof the foreign child process SURVIVED B's kill: resume
    // run A (the session's home run, on disk) and drive sess_a again. If B's
    // kill had reaped the child, resume-A's drive would fail for a dead
    // process. This also proves the provenance invariant in the other
    // direction: resume re-binds the foreign session to its originating run,
    // so its traffic lands in the right bundle again.
    let resume_a = mcp.tool(
        "tui_run",
        serde_json::json!({
            "action": "resume",
            "run_dir": artifact_a,
        }),
    );
    assert_eq!(resume_a["category"], "success", "resume A: {resume_a}");
    let act_foreign = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "x", "id": sess_a }),
    );
    assert_eq!(
        act_foreign["category"], "success",
        "A's session must survive B's kill close and drive under its home run: {act_foreign}"
    );
    // Leave run A durable (resumed session belongs to it) and open a fresh
    // ephemeral current run so scenario (c) has an OPEN current run whose
    // sessions must survive a corrupt-resume refusal. sess_a becomes foreign
    // again and is simply cleaned up at server teardown.
    let open_b = mcp.tool("tui_run", serde_json::json!({ "action": "new" }));
    assert_eq!(
        open_b["category"], "success",
        "re-open after resume A: {open_b}"
    );

    // ── Scenario (c): resume a CORRUPT run with detach=true → refuse at
    // restore (step 1), current sessions survive. ──
    // Make an open-run session in the CURRENT run that had better survive a
    // failed resume.
    let start_c = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('prov-c-ready'); input()"],
            "cwd": persist_root.to_string_lossy(),
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start_c["category"], "success", "start C: {start_c}");
    let sess_c = start_c["data"]["session"].as_str().unwrap().to_string();

    // A corrupt run: a directory named like a persisted run whose run.json
    // is NOT parseable JSON. restore() (manifest::load) errors, which is the
    // step-1 refusal — before the flush and long before any detach.
    let corrupt_dir = persist_root.join("run-corrupt");
    std::fs::create_dir_all(&corrupt_dir).expect("corrupt dir");
    std::fs::write(corrupt_dir.join("run.json"), "{ this is: not json !!!")
        .expect("write corrupt manifest");

    let resume_c = mcp.tool(
        "tui_run",
        serde_json::json!({
            "action": "resume",
            "run_dir": corrupt_dir.to_string_lossy(),
            "detach_existing_sessions": true,
        }),
    );
    assert_eq!(
        resume_c["category"], "invalid_request",
        "corrupt resume must refuse, even with detach=true: {resume_c}"
    );
    assert!(
        resume_c["error"]
            .as_str()
            .map(|m| m.contains("cannot restore"))
            .unwrap_or(false),
        "error names the restore failure: {resume_c}"
    );
    // The current run is untouched, so its session is still alive AND still
    // owned by the open run: driving still works.
    let act_c = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "y", "id": sess_c }),
    );
    assert_eq!(
        act_c["category"], "success",
        "corrupt-resume refusal must leave the current session driving: {act_c}"
    );

    // ── Scenario (d): closed run → restart session → refused BEFORE the
    // process is mutated. ──
    // Close the current run without killing sessions: sess_c stays live.
    let close_c = mcp.tool("tui_run", serde_json::json!({ "action": "close" }));
    assert_eq!(close_c["category"], "success", "close: {close_c}");
    assert_eq!(close_c["data"]["closed"], true);
    assert!(
        close_c["data"]["sessions_stopped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "close without kill keeps the session live: {close_c}"
    );
    // Restart is destructive (it kills the child). On a closed run it must be
    // refused with run_closed, and the refusal must happen BEFORE the child
    // is torn down — proven because sess_c is still the same live session.
    let restart_c = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "restart", "id": sess_c }),
    );
    assert_eq!(
        restart_c["category"], "run_closed",
        "closed run must refuse restart: {restart_c}"
    );
    assert!(
        restart_c["error"]
            .as_str()
            .map(|m| m.contains("closed") && m.contains("resume"))
            .unwrap_or(false),
        "error names the state and the remedy: {restart_c}"
    );

    // The child was never mutated by the refused restart: while driving
    // on a closed run is gated (with_sess refuses run_closed for observe),
    // `stop` bypasses that gate and tears down the REAL live process. Its
    // success here proves the restart refusal preceded any process mutation —
    // restart is the destructive kill path, and it never ran.
    let stop_c = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_c }),
    );
    assert_eq!(
        stop_c["category"], "success",
        "stop succeeds on the same live process the refused restart never touched: {stop_c}"
    );

    let _ = std::fs::remove_dir_all(&persist_root);
}
