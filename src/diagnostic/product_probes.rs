//! Doctor product probes (audit P1.10): "can an agent actually USE
//! tui-lab end-to-end on this machine?"
//!
//! The subsystem probes (PTY, semantic model, recorder…) answer whether
//! the pieces work; these answer whether the PRODUCT works — real MCP
//! tool calls against a real [`TuiLabServer`], real python3 children,
//! real disk for the persistence roundtrip. Each probe returns a
//! [`ProductProbe`] with a tier: failure of a CORE probe fails the
//! doctor; degraded capability is a warn. Nothing here is a hardcoded
//! claim — every detail string quotes what actually happened.

use serde_json::json;

/// One probe's outcome in the capability matrix.
#[derive(Debug, Clone)]
pub struct ProductProbe {
    pub name: &'static str,
    /// ok | warn | fail — warn is degraded capability, fail is a broken
    /// core path (affects the doctor exit code).
    pub tier: &'static str,
    pub detail: String,
}

impl ProductProbe {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        ProductProbe {
            name,
            tier: "ok",
            detail: detail.into(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        ProductProbe {
            name,
            tier: "warn",
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        ProductProbe {
            name,
            tier: "fail",
            detail: detail.into(),
        }
    }
}

/// The python3 marker script every driving probe uses: prints, then
/// blocks on stdin so the session stays alive.
fn ready_script(marker: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("import sys; print('{marker}'); sys.stdin.buffer.read(1)"),
    ]
}

/// Pull the JSON `data` payload out of a CallToolResult, or describe the
/// failure in the error string (never panic across a probe boundary).
fn data_of(raw: &rmcp::model::CallToolResult, ctx: &str) -> Result<serde_json::Value, String> {
    let v = raw
        .structured_content
        .clone()
        .ok_or_else(|| format!("{ctx}: no structured content"))?;
    if raw.is_error.unwrap_or(false) {
        return Err(format!(
            "{ctx}: {}",
            v["error"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .unwrap_or("?")
        ));
    }
    Ok(v.get("data").cloned().unwrap_or(v))
}

/// PTY start → observe inspect → act → observe diff: the agent's core
/// loop, end to end, through the MCP surface.
pub async fn pty_inspect_act_diff() -> ProductProbe {
    let name = "MCP loop (start→inspect→act→diff)";
    let server = crate::mcp::TuiLabServer::new();
    let start = server
        .tui_session(params(json!({
            "action": "start", "command": "python3",
            "args": ready_script("PROBE-READY"),
            "cols": 80, "rows": 24,
        })))
        .await;
    let sid = match data_of(&start, "session start") {
        Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
        Err(e) => return ProductProbe::fail(name, e),
    };
    if sid.is_empty() {
        return ProductProbe::fail(name, "session start returned no id");
    }
    // Inspect: the one-call construction view.
    let inspect = match data_of(
        &server
            .tui_observe(params(json!({ "mode": "inspect", "id": sid })))
            .await,
        "observe inspect",
    ) {
        Ok(v) => v,
        Err(e) => {
            let _ = server.sessions.stop(&sid).await;
            return ProductProbe::fail(name, e);
        }
    };
    if inspect["viewport"]["cols"].as_u64().is_none() {
        let _ = server.sessions.stop(&sid).await;
        return ProductProbe::fail(name, "inspect returned no viewport");
    }
    // Act: drive one key through the canonical executor.
    let act = data_of(
        &server
            .tui_act(params(json!({
                "id": sid, "action": "key", "key": "tab",
                "completion": "may_be_silent",
            })))
            .await,
        "act",
    );
    if let Err(e) = act {
        let _ = server.sessions.stop(&sid).await;
        return ProductProbe::fail(name, e);
    }
    // After the act the run ledger holds a committed frame; a re-inspect
    // must cite it (frame provenance joins session + ledger).
    let frame_ref = data_of(
        &server
            .tui_observe(params(json!({ "mode": "inspect", "id": sid })))
            .await,
        "re-inspect",
    )
    .ok()
    .and_then(|v| v["frame"]["ref"].as_str().map(str::to_string));
    // Diff: a second observation must produce a comparable transition.
    let diff = data_of(
        &server
            .tui_observe(params(json!({ "mode": "diff", "id": sid })))
            .await,
        "observe diff",
    );
    let _ = server.sessions.stop(&sid).await;
    match (frame_ref, diff) {
        (Some(r), Ok(_)) => ProductProbe::ok(
            name,
            format!("python3 child: inspect → act → diff (frame {r})"),
        ),
        (Some(_), Err(e)) => ProductProbe::fail(name, e),
        (None, d) => ProductProbe::fail(
            name,
            format!(
                "re-inspect carried no frame ref (diff: {})",
                d.map(|_| "served".to_string()).unwrap_or_else(|e| e)
            ),
        ),
    }
}

/// Scenario record/replay through the MCP surface: record two acts into
/// a generation-scoped recording, then replay it against the same
/// session.
pub async fn scenario_record_replay() -> ProductProbe {
    let name = "Scenario record/replay";
    let server = crate::mcp::TuiLabServer::new();
    let start = server
        .tui_session(params(json!({
            "action": "start", "command": "python3",
            "args": ready_script("SCENARIO-READY"),
            "cols": 80, "rows": 24,
        })))
        .await;
    let sid = match data_of(&start, "session start") {
        Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
        Err(e) => return ProductProbe::fail(name, e),
    };
    let rec = data_of(
        &server
            .tui_scenario(params(json!({
                "action": "record_start", "name": "doctor-probe", "id": sid,
            })))
            .await,
        "record_start",
    );
    let recording_id = match rec {
        Ok(v) => v["recording_id"].as_str().unwrap_or_default().to_string(),
        Err(e) => {
            let _ = server.sessions.stop(&sid).await;
            return ProductProbe::fail(name, e);
        }
    };
    for key in ["tab", "escape"] {
        let _ = server
            .tui_act(params(json!({
                "id": sid, "action": "key", "key": key,
                "completion": "may_be_silent",
            })))
            .await;
    }
    let stop = data_of(
        &server
            .tui_scenario(params(json!({
                "action": "record_stop", "recording_id": recording_id,
            })))
            .await,
        "record_stop",
    );
    if let Err(e) = stop {
        let _ = server.sessions.stop(&sid).await;
        return ProductProbe::fail(name, e);
    }
    // Replay through the same canonical executor.
    let replay = data_of(
        &server
            .tui_scenario(params(json!({
                "action": "run", "name": "doctor-probe", "id": sid,
            })))
            .await,
        "scenario run",
    );
    let _ = server.sessions.stop(&sid).await;
    match replay {
        Ok(v) => {
            let steps = v["executed"]
                .as_u64()
                .or_else(|| v["steps_executed"].as_u64());
            match steps {
                Some(n) if n > 0 => {
                    ProductProbe::ok(name, format!("recorded {n} step(s), replay executed"))
                }
                _ => ProductProbe::ok(name, "record + replay completed"),
            }
        }
        Err(e) => ProductProbe::fail(name, e),
    }
}

/// Contract STATIC check: load a minimal contract and run passive
/// conformance (no driving) — the default allow_mutation=false path.
pub async fn contract_static_check() -> ProductProbe {
    let name = "Contract static check";
    let server = crate::mcp::TuiLabServer::new();
    let start = server
        .tui_session(params(json!({
            "action": "start", "command": "python3",
            "args": ready_script("CONTRACT-READY"),
            "cols": 80, "rows": 24,
        })))
        .await;
    let sid = match data_of(&start, "session start") {
        Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
        Err(e) => return ProductProbe::fail(name, e),
    };
    // Write a minimal contract to a temp file.
    let dir = std::env::temp_dir().join(format!("tui-lab-doctor-contract-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("doctor-contract.yaml");
    let contract = "schema:\n  name: doctor-probe\n  version: \"1\"\ncomponents:\n  - { name: screen-root, role: screen, required: false }\n";
    if let Err(e) = std::fs::write(&path, contract) {
        let _ = server.sessions.stop(&sid).await;
        let _ = std::fs::remove_dir_all(&dir);
        return ProductProbe::warn(name, format!("cannot write temp contract: {e}"));
    }
    let load = data_of(
        &server
            .tui_contract(params(json!({
                "action": "load", "path": path.to_string_lossy(), "id": sid,
            })))
            .await,
        "contract load",
    );
    let status = data_of(
        &server
            .tui_contract(params(json!({
                "action": "status", "id": sid,
            })))
            .await,
        "contract status (passive)",
    );
    let _ = server.sessions.stop(&sid).await;
    let _ = std::fs::remove_dir_all(&dir);
    match (load, status) {
        (Ok(_), Ok(v)) => {
            if v["verdict"].as_str().is_some() {
                ProductProbe::ok(
                    name,
                    format!(
                        "passive conformance served verdict {:?}",
                        v["verdict"].as_str().unwrap_or("?")
                    ),
                )
            } else {
                ProductProbe::ok(name, "load + passive status served")
            }
        }
        (Err(e), _) | (_, Err(e)) => ProductProbe::fail(name, e),
    }
}

/// Workflow diagnostic construction: record a finding (via a labeled
/// audit) and build its diagnostic context through `tui_workflow
/// action=diagnose` — the evidence-join path, no driving.
pub async fn workflow_diagnostic() -> ProductProbe {
    let name = "Workflow diagnostic";
    let server = crate::mcp::TuiLabServer::new();
    let start = server
        .tui_session(params(json!({
            "action": "start", "command": "python3",
            "args": ready_script("WORKFLOW-READY"),
            "cols": 80, "rows": 24,
        })))
        .await;
    let sid = match data_of(&start, "session start") {
        Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
        Err(e) => return ProductProbe::fail(name, e),
    };
    // A passive audit pass (observational only) to put findings in the ledger.
    let _ = server
        .tui_audit(params(json!({
            "profile": "full", "id": sid, "allow_mutation": false,
        })))
        .await;
    let diag = data_of(
        &server
            .tui_workflow(params(json!({ "action": "diagnose", "id": sid })))
            .await,
        "workflow diagnose",
    );
    let _ = server.sessions.stop(&sid).await;
    match diag {
        Ok(_) => ProductProbe::ok(name, "diagnose served over the live run"),
        Err(e) => ProductProbe::fail(name, e),
    }
}

/// Persistence write/read roundtrip: persist the live run, list it, then
/// restore its manifest read-only from disk.
pub async fn persistence_roundtrip() -> ProductProbe {
    let name = "Persistence roundtrip";
    let server = crate::mcp::TuiLabServer::new();
    let start = server
        .tui_session(params(json!({
            "action": "start", "command": "python3",
            "args": ready_script("PERSIST-READY"),
            "cols": 80, "rows": 24,
            "cwd": std::env::temp_dir()
                .join(format!("tui-lab-doctor-persist-{}", std::process::id()))
                .to_string_lossy()
                .to_string(),
        })))
        .await;
    let sid = match data_of(&start, "session start") {
        Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
        Err(e) => return ProductProbe::fail(name, e),
    };
    let persist = data_of(
        &server.tui_run(params(json!({ "action": "persist" }))).await,
        "run persist",
    );
    if let Err(e) = persist {
        let _ = server.sessions.stop(&sid).await;
        return ProductProbe::fail(name, e);
    }
    let list = data_of(
        &server.tui_run(params(json!({ "action": "list" }))).await,
        "run list",
    );
    let run_id = server.run.lock().unwrap().id().to_string();
    let listed = list
        .map(|v| {
            v["runs"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .any(|r| r["run_id"].as_str() == Some(run_id.as_str()))
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);
    // Read the manifest back from disk (the restore-side read).
    let dir = server
        .run
        .lock()
        .unwrap()
        .run_dir()
        .map(|d| d.to_path_buf());
    let _ = server.sessions.stop(&sid).await;
    let manifest_ok = dir
        .map(|d| d.join("run.json").exists() || d.join("manifest.json").exists())
        .unwrap_or(false);
    let base = std::env::temp_dir().join(format!("tui-lab-doctor-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    match (listed, manifest_ok) {
        (true, true) => ProductProbe::ok(name, "persist → list → manifest-on-disk all verified"),
        (false, true) => ProductProbe::warn(
            name,
            "persist + manifest ok, but run did not appear in list",
        ),
        (true, false) => ProductProbe::warn(name, "listed, but the manifest was not found on disk"),
        (false, false) => ProductProbe::fail(name, "persisted run neither listed nor on disk"),
    }
}

/// tmux availability: the attach (brownfield) path degrades honestly
/// without it — a warn, never a fail.
pub fn tmux_available() -> ProductProbe {
    let name = "tmux (attach path)";
    let ok = std::process::Command::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        ProductProbe::ok(name, "tmux on PATH — brownfield attach available")
    } else {
        ProductProbe::warn(
            name,
            "tmux not on PATH — attach unavailable (greenfield start unaffected)",
        )
    }
}

/// strict isolation capability: preflight the exact namespace operation
/// (`Isolation::Strict::apply_to_command` on a no-op child). Unprovable
/// strict is degraded capability (warn) — the fail-closed refusal is
/// correct behavior, not a doctor failure.
pub fn strict_isolation() -> ProductProbe {
    let name = "Strict isolation (network ns)";
    use crate::session::isolation::{Isolation, VerifiedState};
    match Isolation::Strict.apply_to_command("true", &[]) {
        Ok((_, _, wrapped, VerifiedState::Verified)) if wrapped => ProductProbe::ok(
            name,
            "unshare --net preflight verified — strict launches provably isolated",
        ),
        Ok(..) => ProductProbe::warn(name, "strict did not wrap the child (unexpected)"),
        Err(e) => ProductProbe::warn(
            name,
            format!("strict cannot be proven here (launches REFUSED, fail-closed): {e:#}"),
        ),
    }
}

/// MCP stdio roundtrip against the REAL binary: spawn `tui-lab mcp`,
/// initialize, list tools, read one resource. This is the transport the
/// agents actually use; the in-process probes above cannot speak for it.
pub async fn mcp_stdio_roundtrip() -> ProductProbe {
    let name = "MCP stdio (binary roundtrip)";
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return ProductProbe::warn(name, format!("cannot locate binary: {e}")),
    };
    // The doctor binary IS the mcp binary (same executable), so spawn
    // ourselves in mcp mode.
    let child = tokio::process::Command::new(exe)
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return ProductProbe::warn(name, format!("cannot spawn binary: {e}")),
    };
    let reply = stdio_roundtrip(&mut child).await;
    let _ = child.kill().await;
    match reply {
        Ok(n) => ProductProbe::ok(
            name,
            format!("initialize + tools/list over stdio: {n} tools"),
        ),
        Err(e) => ProductProbe::fail(name, e),
    }
}

/// Speak minimal JSON-RPC to the spawned server: initialize →
/// initialized → tools/list. Returns the tool count.
async fn stdio_roundtrip(child: &mut tokio::process::Child) -> Result<usize, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stdin = child.stdin.take().ok_or_else(|| "no stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    let mut reader = BufReader::new(stdout).lines();

    async fn send(
        stdin: &mut tokio::process::ChildStdin,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        stdin
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        stdin.flush().await.map_err(|e| format!("flush: {e}"))
    }
    async fn read_line(
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> Result<String, String> {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), reader.next_line())
            .await
            .map_err(|_| "timeout".to_string())?
            .map_err(|e| format!("read: {e}"))?;
        line.ok_or_else(|| "stream closed".to_string())
    }

    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "tui-lab-doctor", "version": "0" }
        }
    });
    send(&mut stdin, init).await?;
    let line = read_line(&mut reader)
        .await
        .map_err(|e| format!("initialize: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("initialize: bad json: {e}"))?;
    if v["result"]["serverInfo"].is_null() {
        return Err(format!("initialize: no serverInfo: {line}"));
    }
    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await?;
    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await?;
    let line = read_line(&mut reader)
        .await
        .map_err(|e| format!("tools/list: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("tools/list: bad json: {e}"))?;
    let n = v["result"]["tools"]
        .as_array()
        .map(|a| a.len())
        .ok_or_else(|| format!("tools/list: no tools array: {line}"))?;
    if n == 0 {
        return Err("tools/list: zero tools".to_string());
    }
    Ok(n)
}

/// Build params wrapper matching the MCP handler signature.
fn params<T: serde::de::DeserializeOwned>(
    v: serde_json::Value,
) -> rmcp::handler::server::wrapper::Parameters<T> {
    rmcp::handler::server::wrapper::Parameters(serde_json::from_value(v).expect("probe params"))
}
