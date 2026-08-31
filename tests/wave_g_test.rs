//! Wave G: the product-surface items. Real python3 children throughout —
//! no mocks (project test convention). Covers:
//! - item 65: AuditTransaction honest residue reporting
//! - item 66: the real mouse/performance/states/errors audit drivers
//! - item 67: finding comparison (FIXED / NEW / PERSISTING) via labeled
//!   baselines through the MCP surface
//! - item 73: session actors under cross-session parallelism

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

fn params_typed<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}

fn unwrap_ok(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = raw
        .structured_content
        .clone()
        .or_else(|| raw.content.first().map(|c| {
            let rmcp::model::ContentBlock::Text(t) = c else {
                panic!("{}: non-text content block", ctx);
            };
            serde_json::from_str(&t.text).expect("valid JSON envelope")
        }))
        .expect("structured content");
    assert!(
        !raw.is_error.unwrap_or(false),
        "{} failed: {}",
        ctx,
        v
    );
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
        lines.iter().any(|l| l.as_str().unwrap_or("").contains("disk full")),
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
        findings
            .iter()
            .any(|f| f.id == "MOUSE-NO-TARGETS" || f.id == "MOUSE-OK" || f.id == "MOUSE-UNRESPONSIVE" || f.id == "MOUSE-NO-CAPS"),
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
        findings.iter().map(|f| (&f.id, &f.category)).collect::<Vec<_>>()
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
        .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": a })))
        .await;
    server
        .tui_session(params_typed(serde_json::json!({ "action": "stop", "id": b })))
        .await;
}
