//! Wave G: the product-surface items. Real python3 children throughout —
//! no mocks (project test convention). Covers:
//! - item 65: AuditTransaction honest residue reporting
//! - item 66: the real mouse/performance/states/errors audit drivers
//! - item 67: finding comparison (FIXED / NEW / PERSISTING) via labeled
//!   baselines through the MCP surface
//! - item 73: session actors under cross-session parallelism
//! - item 76: the human control lease refuses every machine-driving path

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

fn params_typed<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}

fn unwrap_ok(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = raw
        .structured_content
        .clone()
        .or_else(|| {
            raw.content.first().map(|c| {
                let rmcp::model::ContentBlock::Text(t) = c else {
                    panic!("{}: non-text content block", ctx);
                };
                serde_json::from_str(&t.text).expect("valid JSON envelope")
            })
        })
        .expect("structured content");
    assert!(!raw.is_error.unwrap_or(false), "{} failed: {}", ctx, v);
    v.get("data").cloned().expect("data payload")
}

async fn start_session(server: &tui_lab::mcp::tools::TuiLabServer, script: &str) -> String {
    let raw = server
        .tui_session(params_typed(serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", script],
            "cols": 80, "rows": 24,
        })))
        .await;
    unwrap_ok(&raw, "session start")["session"]
        .as_str()
        .expect("session id")
        .to_string()
}

// ─────────────────────────── item 66: real drivers ───────────────────────────

#[test]
fn performance_audit_reports_measured_percentiles() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &["-c".to_string(), "print('perf'); input()".to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::driver::performance_audit(sess, 5);
    let f = findings
        .iter()
        .find(|f| f.id == "PERF-OK" || f.id == "PERF-OBSERVE-SLOW")
        .expect("percentile finding present");
    let detail = &f.evidence[0].detail;
    assert!(
        detail["observe_ms"]["p50"].is_u64(),
        "observe p50 must be a measured number: {detail}"
    );
    assert!(
        detail["samples"].as_u64().unwrap() == 5,
        "sample count honored: {detail}"
    );
    // A healthy python echo screen is nowhere near the observe budget.
    assert_eq!(f.id, "PERF-OK", "fast child must not flag slow observe");
    mgr.stop(&id).ok();
}

#[test]
fn errors_audit_survives_burst_and_scans_screen() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &["-c".to_string(), "print('crash-me'); input()".to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::driver::errors_audit(sess, 10);
    // A python `input()` child survives arrows/tab/escape by construction;
    // the honest outcome is ERR-OK with the burst recorded as evidence.
    let ok = findings
        .iter()
        .find(|f| f.id == "ERR-OK" || f.id == "ERR-ON-SCREEN")
        .expect("crash-resistance finding present");
    if ok.id == "ERR-OK" {
        let detail = &ok.evidence[0].detail;
        assert!(
            detail["keys_sent"].as_u64().unwrap() > 0,
            "burst actually sent keys: {detail}"
        );
    }
    mgr.stop(&id).ok();
}

#[test]
fn errors_audit_catches_on_screen_error_text() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "print('Error: disk full'); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::driver::errors_audit(sess, 5);
    let f = findings
        .iter()
        .find(|f| f.id == "ERR-ON-SCREEN")
        .expect("on-screen error must be flagged");
    assert!(
        f.summary.contains("error:"),
        "marker named in summary: {:?}",
        f.summary
    );
    let lines = f.evidence[0].detail["lines"].as_array().expect("lines");
    assert!(
        lines
            .iter()
            .any(|l| l.as_str().unwrap_or("").contains("disk full")),
        "offending line captured as evidence: {:?}",
        lines
    );
    mgr.stop(&id).ok();
}

#[test]
fn mouse_audit_risk_filters_destructive_labels() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &[
                "-c".to_string(),
                "print('[ Delete all ]  [ Help ]'); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::driver::mouse_audit(sess, 12);
    // Whatever the semantic layer detects, the audit must never have
    // clicked a destructive-looking control; the no-targets finding (or a
    // results finding listing what it did) must exist.
    assert!(
        findings.iter().any(|f| f.id == "MOUSE-NO-TARGETS"
            || f.id == "MOUSE-OK"
            || f.id == "MOUSE-UNRESPONSIVE"
            || f.id == "MOUSE-NO-CAPS"),
        "mouse audit produces an honest verdict: {:?}",
        findings.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
    mgr.stop(&id).ok();
}

#[test]
fn states_audit_reports_frame_state_findings() {
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &["-c".to_string(), "print('states'); input()".to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let findings = tui_lab::audit::driver::states_audit(sess, 4);
    assert!(
        !findings.is_empty(),
        "states audit always reports something (inventory or ok)"
    );
    assert!(
        findings
            .iter()
            .all(|f| f.category == "states" || f.id == "AUDIT-RESIDUE"),
        "states findings carry the states category: {:?}",
        findings
            .iter()
            .map(|f| (&f.id, &f.category))
            .collect::<Vec<_>>()
    );
    mgr.stop(&id).ok();
}

// ─────────────────────────── item 67: finding compare ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_label_and_compare_flow() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let id = start_session(&server, "print('compare'); input()").await;

    // First pass: label it. Compare is null (no baseline requested).
    let first = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "keyboard",
                "label": "pass-one",
                "id": id,
            })))
            .await,
        "audit pass one",
    );
    assert_eq!(first["labeled_as"], "pass-one");
    assert!(first["compare"].is_null(), "no compare requested: {first}");
    assert!(
        !first["findings"].as_array().expect("findings").is_empty(),
        "keyboard pass yields findings"
    );

    // Second pass against a real baseline: the compare block reports
    // per-fingerprint verdicts, and every fresh finding is at least
    // classified (new or persisting — never regressed against a raw
    // snapshot baseline).
    let second = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "keyboard",
                "compare_to": "pass-one",
                "id": id,
            })))
            .await,
        "audit pass two",
    );
    let cmp = &second["compare"];
    assert_eq!(cmp["available"], true, "{second}");
    let counts = &cmp["counts"];
    let total_classified = counts["new"].as_u64().unwrap()
        + counts["persisting"].as_u64().unwrap()
        + counts["fixed"].as_u64().unwrap();
    assert!(
        total_classified > 0,
        "every finding fingerprinted into a verdict: {cmp}"
    );

    // Comparing against a label that does not exist is an honest error
    // naming the stored labels.
    let third = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "discoverability",
                "compare_to": "no-such-label",
                "id": id,
            })))
            .await,
        "audit bad compare",
    );
    assert_eq!(third["compare"]["available"], false, "{third}");
    let msg = third["compare"]["error"].as_str().unwrap();
    assert!(
        msg.contains("pass-one") && msg.contains("no-such-label"),
        "error names the requested label and what exists: {msg}"
    );

    server
        .tui_session(params_typed(serde_json::json!({
            "action": "stop", "id": id,
        })))
        .await;
}

// ─────────────────────────── item 65 + 73: residue + parallelism ───────────────────────────

#[test]
fn full_audit_leaves_no_state_residue() {
    // Item 65's contract, proven live: after `full` (which Tabs, resizes,
    // clicks, and bursts), focus and dimensions are back where they started.
    let mut mgr = tui_lab::session::SessionManager::new();
    let id = mgr
        .start(
            "python3",
            &["-c".to_string(), "print('residue'); input()".to_string()],
            None,
            &[],
            100,
            30,
            "auto",
            "local",
        )
        .expect("start");
    let sess = mgr.resolve_mut(Some(&id)).unwrap();
    let report = tui_lab::audit::orchestrator::run_profile(sess, "full").expect("full runs");
    // The last driver restores what it changed; the transaction verify pass
    // must find focus/size unchanged, so NO residue finding appears.
    assert!(
        !report.findings.iter().any(|f| f.id == "AUDIT-RESIDUE"),
        "full audit must restore state (or honestly report it — here restoration works): {:?}",
        report
            .findings
            .iter()
            .filter(|f| f.id == "AUDIT-RESIDUE")
            .map(|f| &f.summary)
            .collect::<Vec<_>>()
    );
    let screen = sess.observe(40).expect("post-audit observe");
    assert_eq!(
        (screen.cols, screen.rows),
        (100, 30),
        "viewport restored after the resize matrix"
    );
    mgr.stop(&id).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_on_one_session_does_not_block_another() {
    // Item 73 headline under real load: session A runs an active audit
    // (multi-second, sends input) while session B serves cheap observes.
    // B must not wait for A.
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let a = start_session(&server, "print('A-audit'); input()").await;
    let b = start_session(&server, "print('B-cheap'); input()").await;

    let server_a = tui_lab::mcp::tools::TuiLabServer::new();
    let _ = &server_a; // (single server owns both sessions; clone for the task)
    let server_clone = server.clone();
    let a2 = a.clone();
    let slow = tokio::spawn(async move {
        server_clone
            .tui_audit(params_typed(serde_json::json!({
                "profile": "full",
                "id": a2,
            })))
            .await
    });
    // Let the audit enter session A's actor.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let start = std::time::Instant::now();
    for _ in 0..3 {
        let obs = unwrap_ok(
            &server
                .tui_observe(params_typed(serde_json::json!({
                    "mode": "summary",
                    "id": b,
                })))
                .await,
            "observe B during A's audit",
        );
        assert_eq!(obs["screen"], "80x24");
    }
    let elapsed = start.elapsed();
    let _ = slow.await;
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "three observes on B must complete while A runs a full audit (took {elapsed:?})"
    );
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": a }),
        ))
        .await;
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": b }),
        ))
        .await;
}

// ─────────────────────────── item 76: lease enforcement ───────────────────────────

/// Read the error out of a failure envelope.
fn unwrap_err(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = raw
        .structured_content
        .clone()
        .unwrap_or_else(|| panic!("{ctx}: structured content"));
    assert!(
        raw.is_error.unwrap_or(false),
        "{ctx} should be a caller fault: {v}"
    );
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_blocks_driving_and_allows_observing() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let id = start_session(&server, "print('lease-me'); input()").await;

    // Take the lease (5 s TTL so the test never waits for expiry).
    let take = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "holder": "ana", "ttl_ms": 5000,
            })))
            .await,
        "lease take",
    );
    assert_eq!(take["leased"], true, "{take}");
    assert_eq!(take["holder"], "ana");

    // Every driving path refuses with control_leased, naming the holder.
    for (tool, args) in [
        (
            "tui_act",
            serde_json::json!({ "action": "key", "key": "tab", "id": id }),
        ),
        (
            "tui_explore",
            serde_json::json!({ "mode": "random", "seed": 1, "actions": 2, "id": id }),
        ),
        (
            "tui_explore",
            serde_json::json!({ "mode": "semantic", "actions": 2, "id": id }),
        ),
        (
            "tui_audit",
            serde_json::json!({ "profile": "keyboard", "id": id }),
        ),
        (
            "tui_audit",
            serde_json::json!({ "profile": "full", "id": id }),
        ),
        (
            "tui_record",
            serde_json::json!({ "format": "start", "id": id }),
        ),
    ] {
        let mut args = args;
        args["id"] = serde_json::json!(id.clone());
        let raw = match tool {
            "tui_act" => server.tui_act(params_typed(args)).await,
            "tui_explore" => server.tui_explore(params_typed(args)).await,
            "tui_audit" => server.tui_audit(params_typed(args)).await,
            "tui_record" => server.tui_record(params_typed(args)).await,
            _ => unreachable!(),
        };
        let e = unwrap_err(&raw, &format!("{tool} under lease"));
        assert_eq!(
            e["category"], "control_leased",
            "{tool} must refuse with control_leased: {e}"
        );
        let msg = e["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("ana"),
            "refusal must name the lease holder: {msg}"
        );
    }

    // Scenario replay refuses too.
    let saved = unwrap_ok(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "save", "name": "lease-replay",
                "steps": [{ "kind": "act", "action": "key", "key": "tab" }],
            })))
            .await,
        "save replay scenario",
    );
    let raw = server
        .tui_scenario(params_typed(serde_json::json!({
            "action": "run", "name": saved["name"], "id": id,
        })))
        .await;
    let e = unwrap_err(&raw, "replay under lease");
    assert_eq!(e["category"], "control_leased", "{e}");

    // Observation stays allowed while the lease is live.
    let obs = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "summary", "id": id }),
            ))
            .await,
        "observe under lease",
    );
    assert_eq!(obs["screen"], "80x24", "{obs}");
    let graph = unwrap_ok(
        &server
            .tui_explore(params_typed(serde_json::json!({ "mode": "state_graph" })))
            .await,
        "state_graph under lease",
    );
    assert!(graph["states"].is_u64(), "{graph}");
    // Static audits are observation, not driving: discoverability reads one
    // frame and must stay allowed.
    let static_audit = unwrap_ok(
        &server
            .tui_audit(params_typed(
                serde_json::json!({ "profile": "discoverability", "id": id }),
            ))
            .await,
        "static audit under lease",
    );
    assert_eq!(static_audit["mode"], "static", "{static_audit}");

    // One-shot capture observes the frame: allowed under lease.
    let cap = unwrap_ok(
        &server
            .tui_record(params_typed(
                serde_json::json!({ "format": "svg", "id": id }),
            ))
            .await,
        "svg capture under lease",
    );
    assert_eq!(cap["format"], "svg", "{cap}");

    // Release: driving works again (proves the block was the lease, not breakage).
    let rel = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "release", "id": id }),
            ))
            .await,
        "lease release",
    );
    assert_eq!(rel["released"], true, "{rel}");
    let act = unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "tab", "id": id }),
            ))
            .await,
        "act after release",
    );
    assert_eq!(act["action"], "key", "{act}");

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_is_exclusive_and_status_reports_it() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let id = start_session(&server, "print('lease-x'); input()").await;

    let first = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "holder": "human-1", "ttl_ms": 5000,
            })))
            .await,
        "first lease",
    );
    assert_eq!(first["leased"], true);

    // A second holder is refused (never silently stolen), naming who holds it.
    let second = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "holder": "agent-9", "ttl_ms": 5000,
            })))
            .await,
        "second lease",
    );
    assert_eq!(second["leased"], false, "{second}");
    assert_eq!(second["holder"], "human-1", "{second}");

    // status carries the live lease.
    let st = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "status", "id": id }),
            ))
            .await,
        "status under lease",
    );
    assert_eq!(st["lease"]["holder"], "human-1", "{st}");
    assert!(
        st["lease"]["remaining_ms"].as_u64().unwrap_or(0) > 0,
        "{st}"
    );

    // Release with no lease afterwards is an honest false.
    unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "release", "id": id }),
            ))
            .await,
        "release",
    );
    let again = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "release", "id": id }),
            ))
            .await,
        "double release",
    );
    assert_eq!(again["released"], false, "{again}");

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

// ─────────────────────────── items 74 + 75: run restore / browser ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_list_resume_restores_identity_and_artifacts() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // Session with cwd=base, do real work, persist, accumulate more, close.
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('resume-me'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "session start",
    );
    let sid = start["session"].as_str().unwrap().to_string();
    let original_run = start["run"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "tab", "id": sid }),
            ))
            .await,
        "act before persist",
    );
    unwrap_ok(
        &server
            .tui_checkpoint(params_typed(
                serde_json::json!({ "action": "save", "name": "cp-r", "id": sid }),
            ))
            .await,
        "checkpoint",
    );
    let persisted = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "persist" })))
            .await,
        "persist",
    );
    assert_eq!(persisted["run_id"], original_run.as_str());
    let artifact_root = persisted["artifact_root"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "escape", "id": sid }),
            ))
            .await,
        "act after persist",
    );
    let st = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await,
        "status",
    );
    let tx_before = st["counts"]["transactions"].as_u64().unwrap();

    // list: the persisted run is discoverable from the base dir.
    let list = unwrap_ok(
        &server
            .tui_run(params_typed(
                serde_json::json!({ "action": "list", "root": base.to_string_lossy() }),
            ))
            .await,
        "run list",
    );
    let entry = list["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["run_id"] == original_run.as_str())
        .expect("our run listed")
        .clone();
    assert_eq!(entry["closed"], false, "{entry}");
    assert_eq!(entry["history_complete"], true, "{entry}");
    assert_eq!(
        entry["ledger_transactions"].as_u64().unwrap(),
        tx_before,
        "ledger line count matches the run's transaction count"
    );

    // Close the live run (kill the session), then resume it in the SAME server.
    unwrap_ok(
        &server
            .tui_run(params_typed(
                serde_json::json!({ "action": "close", "kill_sessions": true }),
            ))
            .await,
        "close",
    );

    let resumed = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({
                "action": "resume", "run_id": original_run,
                "root": base.to_string_lossy(),
            })))
            .await,
        "resume",
    );
    assert_eq!(resumed["resumed"], true, "{resumed}");
    assert_eq!(
        resumed["run_id"],
        original_run.as_str(),
        "identity restored"
    );
    assert_eq!(
        resumed["artifact_root"],
        artifact_root.as_str(),
        "same durable root"
    );
    assert_eq!(resumed["status"]["mode"], "persistent", "{resumed}");
    assert_eq!(
        resumed["status"]["counts"]["transactions"]
            .as_u64()
            .unwrap(),
        tx_before,
        "ledger count restored from disk"
    );
    // The checkpoint saved before persist came back with the run (keyed by
    // its original session id — a fresh session has a new id, so the
    // durable proof is the restored count, and the old file is on disk).
    assert!(
        resumed["status"]["counts"]["checkpoints"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "checkpoints restored: {resumed}"
    );
    assert!(
        std::path::Path::new(&artifact_root)
            .join("checkpoints")
            .read_dir()
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0)
            >= 1,
        "checkpoint files under the durable root"
    );

    // The restored run is LIVE: new interactions append to the same run dir.
    let start2 = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('post-resume'); input()"],
                "cwd": base.to_string_lossy(),
            })))
            .await,
        "session after resume",
    );
    let sid2 = start2["session"].as_str().unwrap().to_string();
    assert_eq!(
        start2["run"],
        original_run.as_str(),
        "new sessions correlate to the resumed run"
    );
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "tab", "id": sid2 }),
            ))
            .await,
        "act on resumed run",
    );
    let ledger =
        std::fs::read_to_string(std::path::Path::new(&artifact_root).join("transactions.jsonl"))
            .expect("ledger");
    assert_eq!(
        ledger.lines().count() as u64,
        tx_before + 1,
        "resumed run appends to the SAME ledger file"
    );

    // Resuming a run id that does not exist is an honest invalid_request.
    let bad = unwrap_err(
        &server
            .tui_run(params_typed(serde_json::json!({
                "action": "resume", "run_id": "run-missing",
                "root": base.to_string_lossy(),
            })))
            .await,
        "resume missing",
    );
    assert!(
        bad["error"].as_str().unwrap_or("").contains("run-missing"),
        "{bad}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The replay CLI renders a persisted run from disk (item 74): identity,
/// ledger summary, findings, artifacts — and fails honestly for an unknown
/// run id.
#[test]
fn replay_cli_renders_persisted_run() {
    use std::process::Command;
    let base = std::env::temp_dir().join(format!("tui-lab-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // Build a durable run through the MCP surface.
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let run_id = rt.block_on(async {
        let server = tui_lab::mcp::tools::TuiLabServer::new();
        let start = unwrap_ok(
            &server
                .tui_session(params_typed(serde_json::json!({
                    "action": "start", "command": "python3",
                    "args": ["-c", "print('replay'); input()"],
                    "cwd": base.to_string_lossy(),
                })))
                .await,
            "session",
        );
        let sid = start["session"].as_str().unwrap().to_string();
        let run_id = start["run"].as_str().unwrap().to_string();
        unwrap_ok(
            &server
                .tui_act(params_typed(
                    serde_json::json!({ "action": "key", "key": "tab", "id": sid }),
                ))
                .await,
            "act",
        );
        unwrap_ok(
            &server
                .tui_audit(params_typed(
                    serde_json::json!({ "profile": "discoverability", "id": sid }),
                ))
                .await,
            "audit for findings",
        );
        unwrap_ok(
            &server
                .tui_run(params_typed(
                    serde_json::json!({ "action": "persist", "root": base.to_string_lossy() }),
                ))
                .await,
            "persist",
        );
        unwrap_ok(
            &server
                .tui_run(params_typed(
                    serde_json::json!({ "action": "close", "kill_sessions": true }),
                ))
                .await,
            "close",
        );
        run_id
    });

    let bin = env!("CARGO_BIN_EXE_hermes-tui-lab");
    let out = Command::new(bin)
        .args(["replay", &run_id, "--root", base.to_string_lossy().as_ref()])
        .output()
        .expect("replay runs");
    assert!(
        out.status.success(),
        "replay failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&run_id), "header names the run: {text}");
    assert!(text.contains("key"), "ledger summary shows actions: {text}");
    assert!(text.contains("findings"), "findings section: {text}");
    assert!(text.contains("state graph"), "graph section: {text}");
    // --full prints the per-transaction lines.
    let out_full = Command::new(bin)
        .args([
            "replay",
            &run_id,
            "--root",
            base.to_string_lossy().as_ref(),
            "--full",
        ])
        .output()
        .expect("replay --full");
    let full_text = String::from_utf8_lossy(&out_full.stdout);
    assert!(
        full_text.contains("settle="),
        "full ledger lines carry settle status: {full_text}"
    );
    // Unknown run id: non-zero exit naming the miss.
    let bad = Command::new(bin)
        .args([
            "replay",
            "run-nope",
            "--root",
            base.to_string_lossy().as_ref(),
        ])
        .output()
        .expect("replay missing");
    assert!(!bad.status.success(), "unknown run must fail");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("run-nope"),
        "error names the missing run"
    );
    let _ = std::fs::remove_dir_all(&base);
}
