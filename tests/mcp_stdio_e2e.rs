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
    let rec = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_start", "name": "e2e-flow" }),
    );
    assert_eq!(rec["category"], "success", "record_start failed: {rec}");
    let _ = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "tab", "id": session }),
    );
    let stop = mcp.tool(
        "tui_scenario",
        serde_json::json!({ "action": "record_stop", "name": "e2e-flow" }),
    );
    assert_eq!(stop["category"], "success", "record_stop failed: {stop}");
    assert_eq!(stop["data"]["steps"], 1, "recorded steps: {stop}");
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

    // --- audit: static profile through stdio ---
    let audit = mcp.tool("tui_audit", serde_json::json!({ "profile": "focus", "id": session }));
    assert_eq!(audit["category"], "success", "audit failed: {audit}");
    assert!(audit["data"]["findings"].is_array());

    // --- stop session ---
    let stop = mcp.tool("tui_session", serde_json::json!({ "action": "stop", "id": session }));
    assert_eq!(stop["category"], "success", "stop failed: {stop}");
}
