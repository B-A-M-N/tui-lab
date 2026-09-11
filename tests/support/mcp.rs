#![allow(dead_code)]

//! Shared MCP stdio transport harness used by integration suites.
//! Every suite drives the actual `tui-lab mcp` binary over JSON-RPC.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared stdout reader: the request loop reads lines from it while a
/// watchdog thread observes progress. A hard hang in the server (e.g. a
/// deadlocked tool handler) would otherwise block `read_line` forever —
/// the deadline was previously only checked between lines, so a stuck
/// server meant a stuck test binary, not a timeout failure.
pub struct SharedStdout(pub BufReader<std::process::ChildStdout>);

pub struct McpProc {
    pub child: Child,
    pub stdin: std::process::ChildStdin,
    pub stdout: Arc<Mutex<SharedStdout>>,
    pub next_id: u64,
}

impl McpProc {
    pub fn spawn() -> Self {
        Self::spawn_in(std::path::Path::new("."))
    }

    pub fn spawn_in(dir: &std::path::Path) -> Self {
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
    pub fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
                Err(e) => {
                    // Audit finding 59: valid JSON-RPC notifications may be
                    // ignored, but NON-JSON stdout is a transport violation
                    // (debug println corrupts the MCP protocol) — fail the
                    // test with the offending line instead of hiding it.
                    panic!("MCP stdio transport violated: non-JSON line on stdout: {e:?} (line follows)\n{line}");
                }
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                watchdog.stop();
                return v;
            }
            // else: notification or out-of-band — keep reading, watchdog
            // stays armed (progress() only slid its deadline)
        }
    }

    pub fn notify(&mut self, method: &str) {
        let msg = serde_json::json!({"jsonrpc":"2.0","method":method});
        writeln!(self.stdin, "{}", msg).expect("write notify");
        self.stdin.flush().expect("flush");
    }

    /// Call one of the tui_* tools and return the parsed result payload
    /// (string content of first text block, parsed as JSON envelope).
    pub fn tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
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
pub struct Watchdog {
    last_line: Arc<Mutex<Instant>>,
    stop: Arc<Mutex<bool>>,
}

impl Watchdog {
    pub fn start(child_pid: Option<u32>, deadline: Instant, liveness: Duration) -> Self {
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
    pub fn progress(&self) {
        *self.last_line.lock().unwrap() = Instant::now();
    }

    /// The matching response arrived; protection ends.
    pub fn stop(&self) {
        *self.stop.lock().unwrap() = true;
    }
}

impl McpProc {
    /// Audit finding 60: transport-level termination. All lifecycle tests
    /// currently stop their sessions through the product API; this catches
    /// a panicked assertion between start and stop so the spawned PTY
    /// cannot leak. `Drop` does not own a `Child` here because Rust struct
    /// drop order is declaration-dependent; call this at test exits.
    pub fn terminate(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Shared helper that performs the MCP initialize handshake and marks the
/// session initialized.
pub fn initialize(mcp: &mut McpProc, client_name: &str) {
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": "0" },
        }),
    );
    assert!(
        init["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {init}"
    );
    mcp.notify("notifications/initialized");
}
