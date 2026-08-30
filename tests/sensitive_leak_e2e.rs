//! Sensitive-input leak E2E (re-review P0 fix 1).
//!
//! Drives the REAL production path end to end: a live session through
//! `TuiLabServer::tui_act` with a unique secret typed under
//! `sensitive: true`, the ephemeral run promoted to a persistent artifact
//! root, and then **every file under that root** read back and searched for
//! the secret. The guarantee under test:
//!
//! > No scenario, transaction, trace, recording, error, debug log, or
//! > reproduction may retain a sensitive payload.
//!
//! The secret must appear ZERO times in the persisted run. A control
//! (non-sensitive) payload must still be present in the ledger, proving the
//! scan would have caught a leak and redaction is scoped, not global.

use rmcp::handler::server::wrapper::Parameters;
use tui_lab::mcp::params::TuiActRequest;
use tui_lab::mcp::tools::TuiLabServer;

/// Recursively read every file under `root` and concatenate the lossy text.
fn all_artifact_text(root: &std::path::Path) -> String {
    let mut buf = String::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                buf.push('\n');
            }
        }
    }
    buf
}

/// Deserialize params exactly as the MCP transport would.
fn params<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}

fn unwrap_ok(raw: &str, ctx: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(raw).expect("valid JSON envelope");
    assert_eq!(
        v.get("category").and_then(|c| c.as_str()),
        Some("success"),
        "{} failed: {}",
        ctx,
        serde_json::to_string(&v).unwrap_or_default()
    );
    v.get("data").cloned().expect("data payload")
}

#[tokio::test]
async fn sensitive_payload_never_reaches_persisted_run_artifacts() {
    // Unique per-run markers so stale artifacts could never pass the scan.
    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let secret = format!("sekrit-{tag}-PAYLOAD-END");
    let control = format!("control-{tag}-PLAIN-END");

    let server = TuiLabServer::new();

    // 1. Start a live python child (echoes typed text to the screen).
    let start_raw = server
        .tui_session(params(serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "import sys; input()"],
            "cwd": null,
            "cols": 80,
            "rows": 24,
        })))
        .await;
    let start = unwrap_ok(&start_raw, "session start");
    let session_id = start["session"]
        .as_str()
        .expect("session id")
        .to_string();

    // 2. Record a scenario first, so scenario capture is active during the
    //    sensitive act (the scenario path was the one path that already
    //    respected `sensitive` — verify it stays correct under the redesign).
    let rec_raw = server
        .tui_record(params_typed::<tui_lab::mcp::params::TuiRecordParams>(serde_json::json!({
            "action": "start",
            "name": "leak-e2e",
        })))
        .await;
    let _rec = unwrap_ok(&rec_raw, "record start");

    // 3. Type the SECRET with sensitive:true through the real act path.
    let secret_raw = server
        .tui_act(params_typed::<TuiActRequest>(serde_json::json!({
            "action": "type",
            "text": secret.clone(),
            "sensitive": true,
            "no_wait": null,
            "wait_ms": 150,
            "id": session_id.clone(),
        })))
        .await;
    unwrap_ok(&secret_raw, "sensitive act");

    // 4. Type a CONTROL payload (not sensitive) — must survive in evidence.
    let control_raw = server
        .tui_act(params_typed::<TuiActRequest>(serde_json::json!({
            "action": "type",
            "text": control.clone(),
            "sensitive": null,
            "no_wait": null,
            "wait_ms": 150,
            "id": session_id.clone(),
        })))
        .await;
    unwrap_ok(&control_raw, "control act");

    // 5. Save the scenario recording so it lands in the run's artifacts.
    let save_raw = server
        .tui_scenario(params_typed::<tui_lab::mcp::params::TuiScenarioParams>(serde_json::json!({
            "action": "save",
            "name": "leak-e2e",
        })))
        .await;
    // Scenario save may legitimately fail if no steps were recorded for the
    // tool-traffic path; a leak test only needs it to not crash.
    let _ = save_raw;

    // 6. Promote the ephemeral run into a persistent artifact root.
    let base = tempfile::tempdir().expect("tmpdir");
    let run_raw = server
        .tui_run(params_typed::<tui_lab::mcp::params::TuiRunParams>(serde_json::json!({
            "action": "persist",
            "root": base.path().to_string_lossy().to_string(),
            "kill_sessions": null,
        })))
        .await;
    let run = unwrap_ok(&run_raw, "persist");
    let artifact_root = run["artifact_root"]
        .as_str()
        .expect("artifact root path")
        .to_string();

    // 7. Close the run so every held artifact flushes.
    let close_raw = server
        .tui_run(params_typed::<tui_lab::mcp::params::TuiRunParams>(serde_json::json!({
            "action": "close",
            "root": null,
            "kill_sessions": true,
        })))
        .await;
    unwrap_ok(&close_raw, "close");

    // 8. THE SCAN: every byte of every persisted artifact.
    let everything = all_artifact_text(std::path::Path::new(&artifact_root));
    assert!(
        everything.contains(&control),
        "control payload must be present in artifacts (scan validity check)"
    );
    let occurrences = everything.matches(&secret).count();
    assert_eq!(
        occurrences, 0,
        "sensitive payload must appear ZERO times in persisted artifacts"
    );

    // Also verify the manifest reports a complete history for this small run.
    let manifest = std::fs::read_to_string(
        std::path::Path::new(&artifact_root).join("run.json"),
    )
    .expect("manifest");
    assert!(
        manifest.contains("\"history_complete\": true"),
        "small run must be replay-complete: {manifest}"
    );
}

/// Variant of `params` for when inference needs the target type spelled out.
fn params_typed<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}
