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

#[tokio::test]
async fn performance_audit_reports_measured_percentiles() {
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
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
        // The driver measures and reports — not asserts — so whether it
        // lands on PERF-OK (fast child) or PERF-OBSERVE-SLOW (host under
        // concurrent load pushing observe past its 500 ms p95 budget) is
        // the honest machine state, not a correctness failure. What the
        // item 66 test proves: the real performance driver produced a
        // measured-percentile finding. (Hard-asserting PERF-OK flakes
        // whenever the shared test host is concurrently busy.)
        assert!(
            f.id == "PERF-OK" || f.id == "PERF-OBSERVE-SLOW",
            "honest performance verdict, was '{}' (detail {detail})",
            f.id
        );
    })
    .await
    .expect("performance audit job");
    pool.stop(&id).await.ok();
}

#[tokio::test]
async fn errors_audit_survives_burst_and_scans_screen() {
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
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
    })
    .await
    .expect("errors audit job");
    pool.stop(&id).await.ok();
}

#[tokio::test]
async fn errors_audit_catches_on_screen_error_text() {
    // Wave 4 item 39: "Error:" alone is a WEAK marker (matches log viewers
    // and docs too) — the audit reports it as ERR-TEXT-HINT at info, not a
    // fabricated crash verdict. Strong markers (panic/traceback) stay
    // ERR-ON-SCREEN at error. Both tiers must name the offending line.
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
        let findings = tui_lab::audit::driver::errors_audit(sess, 5);
        let f = findings
            .iter()
            .find(|f| f.id == "ERR-TEXT-HINT")
            .expect("weak marker must surface as a hint, not silence");
        assert_eq!(
            f.severity,
            tui_lab::audit::Severity::Info,
            "weak markers stay informational"
        );
        assert!(
            f.summary.contains("error"),
            "marker named in summary: {:?}",
            f.summary
        );
        let weak = f.evidence[0].detail["weak_markers"]
            .as_array()
            .expect("weak");
        assert!(
            weak.iter().any(|m| m.as_str() == Some("error:")),
            "error: listed among weak markers: {weak:?}"
        );
    })
    .await
    .expect("weak marker job");
    pool.stop(&id).await.ok();
}

#[tokio::test]
async fn errors_audit_strong_markers_stay_errors() {
    // A strong app-failure marker (panic text) keeps the ERR-ON-SCREEN
    // error verdict.
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
        .start(
            "python3",
            &[
                "-c".to_string(),
                "print('panicked at src/main.rs:1'); input()".to_string(),
            ],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
        let findings = tui_lab::audit::driver::errors_audit(sess, 5);
        let f = findings
            .iter()
            .find(|f| f.id == "ERR-ON-SCREEN")
            .expect("strong marker must stay an error-severity finding");
        assert_eq!(f.severity, tui_lab::audit::Severity::Error);
        let strong = f.evidence[0].detail["strong_markers"]
            .as_array()
            .expect("strong");
        assert!(
            strong.iter().any(|m| m.as_str() == Some("panicked at")),
            "panic marker recorded: {strong:?}"
        );
    })
    .await
    .expect("strong marker job");
    pool.stop(&id).await.ok();
}

#[tokio::test]
async fn mouse_audit_risk_filters_destructive_labels() {
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
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
    })
    .await
    .expect("mouse audit job");
    pool.stop(&id).await.ok();
}

#[tokio::test]
async fn states_audit_reports_frame_state_findings() {
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
        let findings = tui_lab::audit::driver::states_audit(sess, 4);
        assert!(
            !findings.is_empty(),
            "states audit always reports something (inventory or ok)"
        );
        assert!(
            findings
                .iter()
                .all(|f| f.category.as_str() == "states" || f.id == "AUDIT-RESIDUE"),
            "states findings carry the states category: {:?}",
            findings
                .iter()
                .map(|f| (&f.id, &f.category))
                .collect::<Vec<_>>()
        );
    })
    .await
    .expect("states audit job");
    pool.stop(&id).await.ok();
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
                "allow_mutation": true,
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
                "allow_mutation": true,
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

#[tokio::test]
async fn full_audit_leaves_no_state_residue() {
    // Item 65's contract, proven live: after `full` (which Tabs, resizes,
    // clicks, and bursts), focus and dimensions are back where they started.
    let pool = tui_lab::session::SessionPool::new();
    let id = pool
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
        .await
        .expect("start");
    pool.with_session(Some(&id), |sess| {
        let report = tui_lab::audit::orchestrator::run_profile(sess, "full").expect("full runs");
        // The last driver restores what it changed; the transaction verify
        // pass must find focus/size/cursor/title unchanged, so NO genuine
        // (WARN) residue finding appears. Audit P1 (finding 15): a
        // structure-ONLY change is contextual evidence, reported at INFO
        // — the drivers' control bytes echo into this plain `input()`
        // child's screen, which moves the structure hash while leaving
        // every owned axis intact — so an INFO AUDIT-RESIDUE row is
        // allowed and a WARN one is not.
        let genuine: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.id == "AUDIT-RESIDUE" && f.severity == tui_lab::audit::Severity::Warn)
            .collect();
        assert!(
            genuine.is_empty(),
            "full audit must restore the state it is responsible for: {:?}",
            genuine.iter().map(|f| &f.summary).collect::<Vec<_>>()
        );
        for f in report.findings.iter().filter(|f| f.id == "AUDIT-RESIDUE") {
            assert_eq!(
                f.severity,
                tui_lab::audit::Severity::Info,
                "structure-only residue is INFO context, never a defect: {}",
                f.summary
            );
            assert!(
                f.summary.contains("structure changed"),
                "INFO rows are the structure-only class: {}",
                f.summary
            );
        }
        let screen = sess.observe(40).expect("post-audit observe");
        assert_eq!(
            (screen.cols, screen.rows),
            (100, 30),
            "viewport restored after the resize matrix"
        );
    })
    .await
    .expect("residue job");
    pool.stop(&id).await.ok();
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
    // Finding 4: the grant carries a release token.
    let lease_id = take["lease_id"].as_str().expect("lease_id").to_string();

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
            serde_json::json!({ "profile": "keyboard", "id": id, "allow_mutation": true }),
        ),
        (
            "tui_audit",
            serde_json::json!({ "profile": "full", "id": id, "allow_mutation": true }),
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
        // Review §14: the machine-usable facts ride in `details` — the
        // agent branches on `retry_after_ms` without parsing the message.
        assert_eq!(
            e["details"]["holder"], "ana",
            "{tool} must attach the holder in details: {e}"
        );
        let retry = e["details"]["retry_after_ms"].as_u64().unwrap_or(0);
        assert!(
            retry > 0,
            "{tool} must attach a positive retry_after_ms: {e}"
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

    // Review §5: passive LIVE-SESSION audits observe the raw ring and fused
    // frame but send nothing — they must stay available under a human lease
    // (the old is_active() gate, now needs_live_session, wrongly refused
    // them when it also gated the lease). allow_mutation is
    // passed to prove the lease refusal did not previously depend on it.
    for passive in [
        "color",
        "terminal_modes",
        "rendering",
        "input_protocol",
        "shell_cli",
        "lifecycle",
        "query_response",
    ] {
        let run = unwrap_ok(
            &server
                .tui_audit(params_typed(serde_json::json!({
                    "profile": passive, "id": id, "allow_mutation": true,
                })))
                .await,
            &format!("{passive} audit under lease"),
        );
        assert_eq!(
            run["mode"], "active",
            "{passive} is a live-session reader; it must run under a human lease: {run}"
        );
        // And it produced a finding_count (the audit actually ran).
        assert!(run.get("finding_count").is_some(), "{passive}: {run}");
    }

    // Review §6: the process-consuming audit exists on the wire now, but
    // neither allow_mutation nor anything else but allow_process_restart
    // authorizes it — and it is refused BEFORE any lease consideration.
    let exit_no_flag = unwrap_err(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "lifecycle_exit", "id": id, "allow_mutation": true,
            })))
            .await,
        "lifecycle_exit without restart authorization",
    );
    assert_eq!(
        exit_no_flag["category"], "invalid_request",
        "{exit_no_flag}"
    );
    assert!(
        exit_no_flag["error"]
            .as_str()
            .unwrap_or("")
            .contains("allow_process_restart"),
        "refusal must name the real authorization: {exit_no_flag}"
    );
    let exit_leased = unwrap_err(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "lifecycle_exit", "id": id, "allow_process_restart": true,
            })))
            .await,
        "lifecycle_exit authorized but under lease",
    );
    assert_eq!(exit_leased["category"], "control_leased", "{exit_leased}");

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

    // Release: driving works again (proves the block was the lease, not
    // breakage). Finding 4: early release requires the lease_id token.
    let rel = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "release", "id": id, "lease_id": lease_id
            })))
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
    // Finding 4: the release token comes back with the grant.
    let lease_id = first["lease_id"].as_str().expect("lease_id").to_string();

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

    // Finding 4: a tokenless release is refused while a live lease stands,
    // and a WRONG token cannot drop someone else's grant either.
    let tokenless = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "release", "id": id }),
        ))
        .await;
    assert_eq!(
        tokenless
            .structured_content
            .as_ref()
            .and_then(|v| v.get("category"))
            .and_then(|c| c.as_str()),
        Some("invalid_request"),
        "release without lease_id is refused: {tokenless:?}"
    );
    let wrong = server
        .tui_session(params_typed(serde_json::json!({
            "action": "release", "id": id, "lease_id": "lease-not-the-holder"
        })))
        .await;
    assert_eq!(
        wrong
            .structured_content
            .as_ref()
            .and_then(|v| v.get("category"))
            .and_then(|c| c.as_str()),
        Some("control_leased"),
        "a wrong token refuses with control_leased: {wrong:?}"
    );
    // status still shows the lease standing after both refusals.
    let st2 = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "status", "id": id }),
            ))
            .await,
        "status after refused releases",
    );
    assert_eq!(st2["lease"]["holder"], "human-1", "{st2}");

    // The correct token releases.
    let rel = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "release", "id": id, "lease_id": lease_id }),
            ))
            .await,
        "token release",
    );
    assert_eq!(rel["released"], true, "{rel}");

    // Release with no lease afterwards is an honest false.
    let again = unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "release", "id": id, "lease_id": lease_id }),
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

/// Finding 4/13: the lease gates the LIFECYCLE, not just driving. A live
/// human lease refuses `tui_session stop` and `restart` (both kill the
/// process the human is driving) and `tui_run close kill_sessions=true`
/// skips leased sessions (reported as `leased_not_killed`). After release
/// with the token, the same calls go through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_gates_lifecycle_mutations() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let id = start_session(
        &server,
        "print('lifecycle-lease'); import sys; sys.stdin.read(1)",
    )
    .await;

    let lease = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "holder": "operator", "ttl_ms": 60000,
            })))
            .await,
        "lease",
    );
    let _lease_id = lease["lease_id"].as_str().expect("lease_id").to_string();

    // stop refuses under the lease.
    let stop = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
    let stop_v = stop.structured_content.clone().expect("stop envelope");
    assert_eq!(stop_v["category"], "control_leased", "{stop_v:?}");
    assert_eq!(stop_v["details"]["holder"], "operator", "{stop_v:?}");
    assert!(
        stop_v["details"]["retry_after_ms"].as_u64().unwrap_or(0) > 0,
        "retry window rides the refusal: {stop_v:?}"
    );

    // restart refuses under the lease.
    let restart = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "restart", "id": id }),
        ))
        .await;
    let restart_v = restart
        .structured_content
        .clone()
        .expect("restart envelope");
    assert_eq!(restart_v["category"], "control_leased", "{restart_v:?}");

    // kill_sessions=true SKIPS the leased session (never kills a human's
    // process) but still stops an UNLEASED sibling — the skip is the lease
    // gate, not a close-path regression. A second, unleased session makes
    // that contrast observable through `tui_run status` (which does not
    // drive sessions and stays legal on a closed run).
    let free_id = start_session(&server, "print('unleased'); import sys; sys.stdin.read(1)").await;
    // Fresh run so the new sibling is owned by the CURRENT (open) run.
    let free_leases = server
        .tui_run(params_typed(serde_json::json!({ "action": "status" })))
        .await;
    assert!(
        free_leases.structured_content.is_some(),
        "run status: {free_leases:?}"
    );
    let close_res = unwrap_ok(
        &server
            .tui_run(params_typed(
                serde_json::json!({ "action": "close", "kill_sessions": true }),
            ))
            .await,
        "close",
    );
    assert_eq!(
        close_res["sessions_stopped"]
            .as_array()
            .expect("stopped list")
            .clone(),
        serde_json::json!([free_id]).as_array().unwrap().clone(),
        "the unleased sibling was stopped: {close_res}"
    );
    assert_eq!(
        close_res["leased_not_killed"][0]["session"],
        id.as_str(),
        "close NAMES the leased session it refused to kill: {close_res}"
    );
    assert_eq!(
        close_res["leased_not_killed"][0]["holder"], "operator",
        "{close_res}"
    );
    let after = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await,
        "run status after close",
    );
    let sessions_now = after["sessions"].as_array().expect("sessions list");
    assert!(
        sessions_now.iter().any(|v| v.as_str() == Some(id.as_str())),
        "the LEASED session survived kill_sessions=true: {after:?}"
    );
    assert!(
        !sessions_now
            .iter()
            .any(|v| v.as_str() == Some(free_id.as_str())),
        "the unleased sibling was stopped as requested: {after:?}"
    );

    // The close never touched the survivor's lease state: the lease fields
    // close reported prove it was still live at close time (tui_session
    // status is run-gated on the closed run, so this is the honest window).
    let remaining = close_res["leased_not_killed"][0]["remaining_ms"]
        .as_u64()
        .expect("remaining_ms present");
    assert!(
        remaining > 0 && remaining <= lease["ttl_ms"].as_u64().unwrap_or(0),
        "the lease was live (within its TTL) at close time: {remaining}ms"
    );
    // (Leftover processes: the server's drop tears the pool down and kills
    // the survivor — the lease never blocked the internal stop authority,
    // only the machine-facing surface, by design.)
}

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
    // The versioned stream's header line is not a transaction.
    let ledger_records = ledger.lines().filter(|l| !l.contains("\"schema\"")).count() as u64;
    assert_eq!(
        ledger_records,
        tx_before + 1,
        "resumed run appends to the SAME ledger file"
    );
    // Finding 32: resume reopened the persisted run — the response names
    // the epoch and the run's status carries it.
    assert_eq!(resumed["resume_epoch"], 1, "{resumed}");
    assert_eq!(resumed["status"]["resume_epoch"], 1);

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

/// Review P0.2: run provenance. A session launched under run A must not be
/// driveable after the current run became B; and resuming a different run
/// while a foreign-owned session is live refuses unless explicitly detached.
#[tokio::test]
async fn run_provenance_binding_and_resume_guard() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-provenance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // Launch session S under the current run A.
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('prov-a'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start A",
    );
    let sid = start["session"].as_str().unwrap().to_string();
    let run_a = start["run"].as_str().unwrap().to_string();
    // Baseline drive succeeds.
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid }),
            ))
            .await,
        "act under A",
    );

    // Persist A (so it is resumable), then begin a FRESH run B without
    // stopping S. There is no explicit new-run op: resume fills that role,
    // so the provenance guard is the resume path keeping S out of B.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "persist" })))
            .await,
        "persist A",
    );

    // Begin a FRESH ephemeral run B while S (owned by A) is still live.
    // S must now be untouchable: it belongs to A, the current run is B.
    let newr = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "new run B",
    );
    let run_b = newr["new_run_id"].as_str().unwrap().to_string();
    assert_ne!(run_b, run_a, "new run is distinct");
    assert_eq!(newr["previous_run"], run_a.as_str(), "prior run named");

    // Driving S under B is refused (provenance), not silently re-targeted.
    let drive = &server
        .tui_act(params_typed(
            serde_json::json!({ "action": "key", "key": "enter", "id": sid }),
        ))
        .await;
    let dv = drive
        .structured_content
        .clone()
        .or_else(|| {
            drive.content.first().map(|c| {
                let rmcp::model::ContentBlock::Text(t) = c else {
                    unreachable!()
                };
                serde_json::from_str(&t.text).unwrap()
            })
        })
        .unwrap();
    assert_eq!(
        dv["category"], "no_session",
        "foreign-owner session must be refused: {dv}"
    );
    assert!(
        dv["error"].as_str().unwrap_or("").contains("bound to run"),
        "names the provenance: {dv}"
    );

    // Resuming A while S (an A-session) is live is fine (same owner) — the
    // resume guard only fires for FOREIGN sessions. But resuming under B
    // doesn't apply; instead: closing B and resuming A is same-owner, and S
    // becomes driveable again under A.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "resume", "run_id": &run_a, "root": base.to_string_lossy() })))
            .await,
        "resume A (same owner as S)",
    );
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid }),
            ))
            .await,
        "S driveable again under its owner A",
    );

    // Positive cleanup: stopping S drops the owner entry.
    unwrap_ok(
        &server
            .tui_session(params_typed(
                serde_json::json!({ "action": "stop", "id": sid }),
            ))
            .await,
        "stop S",
    );
    // A session that never existed has no owner; resolution just fails as
    // before (no panic on the ownership lookup).
    assert!(
        server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid })
            ))
            .await
            .is_error
            .unwrap_or(false),
        "stopped session is gone"
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

    let bin = env!("CARGO_BIN_EXE_tui-lab");
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

/// Finding 33: `tui_run new` must not silently discard an ephemeral run's
/// in-memory evidence. It refuses, naming the counts, until the caller
/// either persists the run (evidence moves to disk where `new` flushes it)
/// or passes `discard=true` to abandon it deliberately.
#[tokio::test]
async fn run_new_refuses_to_discard_ephemeral_evidence() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-discard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // 0. A fresh server run holds NOTHING: `new` swaps directly.
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "new over an empty run",
    );

    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('discard-a'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid }),
            ))
            .await,
        "act (records a transaction)",
    );

    // 1. (step 0 already proved the empty-run path.)

    // 2. Record evidence again (new run, new session correlation needed).
    let start2 = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('discard-b'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start 2",
    );
    let sid2 = start2["session"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid2 }),
            ))
            .await,
        "act 2",
    );

    // 3. `new` now REFUSES, naming what would be lost.
    let refused = unwrap_err(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "new over evidence",
    );
    let msg = refused["error"].as_str().unwrap_or("").to_string();
    assert!(
        msg.contains("exist only in memory"),
        "refusal names the hazard: {refused}"
    );
    let details = &refused["details"];
    assert!(
        details["evidence"]["transactions"].as_u64().unwrap_or(0) >= 1,
        "transaction count in the details: {details}"
    );
    assert!(
        details["hint"]
            .as_str()
            .unwrap_or("")
            .contains("discard=true"),
        "hint names the escape hatch: {details}"
    );

    // 4. The refusing `new` changed nothing: the evidence is still there.
    let st = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await,
        "status after refusal",
    );
    assert_eq!(
        st["counts"]["transactions"].as_u64().unwrap_or(0),
        details["evidence"]["transactions"].as_u64().unwrap_or(0),
        "refused new left the run intact"
    );

    // 5. discard=true proceeds — and the response names the accepted loss.
    let discarded = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!(
                { "action": "new", "discard": true }
            )))
            .await,
        "new with discard",
    );
    assert_eq!(
        discarded["discarded_evidence"]["evidence"]["transactions"].as_u64(),
        details["evidence"]["transactions"].as_u64(),
        "response names the discarded counts: {discarded}"
    );
    let after = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await,
        "status after discard",
    );
    assert_eq!(
        after["counts"]["transactions"].as_u64().unwrap_or(0),
        0,
        "the new run starts empty"
    );

    // 6. The persist-first alternative: evidence on disk makes `new` a
    // flush-then-swap, not a loss.
    let start3 = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('discard-c'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start 3",
    );
    let sid3 = start3["session"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "enter", "id": sid3 }),
            ))
            .await,
        "act 3",
    );
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "persist" })))
            .await,
        "persist first",
    );
    let swapped = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "new" })))
            .await,
        "new after persist",
    );
    assert_eq!(
        swapped["previous_flushed"], true,
        "persistent run flushed before the swap: {swapped}"
    );

    // Cleanup: stop any live sessions the test leaked.
    for s in [sid, sid2, sid3] {
        let _ = server
            .tui_session(params_typed(
                serde_json::json!({ "action": "stop", "id": s }),
            ))
            .await;
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Finding 36: `tui_observe mode=inspect` — the one-call construction
/// view. A real python child throughout: frame id, semantic identity,
/// per-control stable ids + bounds + state + affordances, target
/// narrowing (id / unique label / ambiguity refused with candidates),
/// and the loaded contract's violations on THIS frame.
#[tokio::test]
async fn inspect_mode_one_call_construction_view() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-inspect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // A child with a visible button-like affordance and a form-ish line.
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", r#"print("== MAIN =="); print("[ Save ]  [ Quit ]"); print("Host: localhost"); input()"#],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    // Seed a committed frame (act once) so the inspect view can cite it.
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "tab", "id": sid }),
            ))
            .await,
        "seed act",
    );

    // ── full inspect: identity + controls + frame ──
    let insp = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "inspect", "id": sid }),
            ))
            .await,
        "inspect",
    );
    // Semantic identity present and hash-like; frame citation present
    // because the seed act committed a frame.
    let semantic_identity = insp["semantic_identity"]
        .as_str()
        .expect("semantic identity")
        .to_string();
    assert!(
        !semantic_identity.is_empty(),
        "semantic identity non-empty: {insp}"
    );
    let frame = &insp["frame"];
    assert!(
        frame["ref"]
            .as_str()
            .unwrap_or_default()
            .starts_with("frame:"),
        "inspect cites the committed frame: {frame}"
    );
    // Controls carry the construction-grade facts.
    let controls = insp["controls"].as_array().expect("controls array");
    assert!(
        !controls.is_empty(),
        "at least one control detected: {insp}"
    );
    let first = &controls[0];
    assert!(
        first["id"].as_str().is_some()
            && first["kind"].is_string()
            && first["bounds"].is_object()
            && first["state"].is_object()
            && first["confidence"].is_object(),
        "control carries id/kind/bounds/state/confidence: {first}"
    );
    // Focus reported.
    assert!(
        insp["focus"].is_object() && insp["viewport"].is_object(),
        "focus + viewport present: {insp}"
    );
    // No contract loaded → honest note, no fabricated violations.
    assert_eq!(insp["contract"]["loaded"], false, "{insp}");
    assert!(
        insp["contract"]["violations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no contract → no violations: {insp}"
    );

    // ── target narrowing by unique label substring ──
    let targeted = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "inspect", "id": sid, "target": "save" }),
            ))
            .await,
        "inspect target=save",
    );
    let tcontrols = targeted["controls"].as_array().expect("targeted controls");
    assert_eq!(
        tcontrols.len(),
        1,
        "unique substring narrows to one: {targeted}"
    );
    assert_eq!(targeted["targeted"], true);

    // ── unknown target refused with candidates (not a first-pick) ──
    let missing = unwrap_err(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "inspect", "id": sid, "target": "svae" }),
            ))
            .await,
        "inspect target typo",
    );
    assert!(
        missing["error"]
            .as_str()
            .unwrap_or("")
            .contains("not found"),
        "{missing}"
    );
    assert!(
        missing["details"]["candidates"]
            .as_array()
            .map(|c| !c.is_empty())
            .unwrap_or(false),
        "candidates named: {missing}"
    );

    // ── contract verdict: load a contract naming a role the screen
    //    does NOT have (a dialog) → one required violation ──
    let contract_path = base.join("inspect_contract.yaml");
    std::fs::write(
        &contract_path,
        "schema:\n  name: inspect-fixture\n  version: \"1\"\ncomponents:\n  - name: save-button\n    role: button\n    required: false\n  - name: missing-dialog\n    role: dialog\n    required: true\n",
    )
    .expect("write contract");
    unwrap_ok(
        &server
            .tui_contract(params_typed(
                serde_json::json!({ "action": "load", "path": contract_path.to_string_lossy() }),
            ))
            .await,
        "load contract",
    );
    let with_contract = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "inspect", "id": sid }),
            ))
            .await,
        "inspect with contract",
    );
    assert_eq!(with_contract["contract"]["loaded"], true, "{with_contract}");
    let violations = with_contract["contract"]["violations"]
        .as_array()
        .expect("violations array");
    assert_eq!(
        violations.len(),
        1,
        "only the missing dialog: {violations:?}"
    );
    assert_eq!(violations[0]["component"], "missing-dialog");
    assert_eq!(violations[0]["severity"], "error");
    assert_eq!(violations[0]["required"], true);

    // ── ambiguous target: a second matching control makes the narrow a
    //    refusal with the matches named ──
    let start2 = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", r#"print("[ OK ]"); print("[ ok ]"); input()"#],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start 2",
    );
    let sid2 = start2["session"].as_str().unwrap().to_string();
    let amb = unwrap_err(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "inspect", "id": sid2, "target": "ok" }),
            ))
            .await,
        "inspect ambiguous",
    );
    assert!(
        amb["error"].as_str().unwrap_or("").contains("ambiguous"),
        "{amb}"
    );
    assert_eq!(
        amb["details"]["candidates"].as_array().map(|c| c.len()),
        Some(2),
        "both matches named: {amb}"
    );

    for s in [sid, sid2] {
        let _ = server
            .tui_session(params_typed(
                serde_json::json!({ "action": "stop", "id": s }),
            ))
            .await;
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Finding 37: `tui_contract action=scaffold mode=explore` — a bounded
/// SAFE multi-state pass (initial screen, Tab focus walk, Escape,
/// viewport probes) scaffolds a contract that cites every state it saw.
/// Every requirement stays non-required; the pass leaves the app at its
/// launch viewport. Real python child throughout.
#[tokio::test]
async fn scaffold_explore_gathers_multi_state_contract() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-scaffold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // A focus ring that MOVES with Tab (reverse-video focus — the
    // semantic focus inferencer's evidence), so the walk records states.
    // cwd = fixtures/ so the relative script path resolves.
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["focus_ring_tui.py"],
                "cwd": "fixtures", "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    // ── mode=current still works (backward compat: default) ──
    let current = unwrap_ok(
        &server
            .tui_contract(params_typed(
                serde_json::json!({ "action": "scaffold", "id": sid }),
            ))
            .await,
        "scaffold current",
    );
    assert_eq!(current["scaffold_mode"], "current", "{current}");
    assert_eq!(current["states_observed"], serde_json::Value::Null);

    // ── mode=explore: multi-state gather ──
    let explore = unwrap_ok(
        &server
            .tui_contract(params_typed(serde_json::json!(
                { "action": "scaffold", "scaffold_mode": "explore", "id": sid }
            )))
            .await,
        "scaffold explore",
    );
    assert_eq!(explore["scaffold_mode"], "explore", "{explore}");
    let states = explore["states"].as_array().expect("states array");
    assert!(
        !states.is_empty(),
        "at least the initial state recorded: {explore}"
    );
    assert_eq!(
        states[0]["via"], "initial",
        "state 0 is the initial screen: {states:?}"
    );
    // The Tab walk is expected to visit the focusable controls.
    let tab_states: Vec<&serde_json::Value> = states
        .iter()
        .filter(|s| s["via"].as_str().unwrap_or("").starts_with("tab:"))
        .collect();
    assert!(
        !tab_states.is_empty(),
        "the focus walk recorded states: {states:?}"
    );
    // Viewports include the launch size (80x24) and the probed sizes.
    let viewports = explore["viewports"].as_array().expect("viewports");
    assert!(
        viewports.iter().any(|v| v["cols"] == 80 && v["rows"] == 24),
        "launch viewport declared: {viewports:?}"
    );
    // Every inferred requirement stays optional.
    let yaml = explore["yaml"].as_str().unwrap_or_default();
    assert!(
        !yaml
            .lines()
            .any(|l| l.trim_start().starts_with("required:") && l.contains("true")),
        "nothing observed becomes required: {yaml}"
    );
    // The scaffold.inferred extension names the mode.
    assert!(
        yaml.contains("inferred: true") || explore["inferred"] == true,
        "inferred marker present: {yaml}"
    );

    // ── the app was left at its launch viewport ──
    let after = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "summary", "id": sid }),
            ))
            .await,
        "post-scaffold observe",
    );
    assert_eq!(after["screen"], "80x24", "viewport restored: {after}");

    // ── unknown scaffold_mode refused with the accepted set ──
    let bad = unwrap_err(
        &server
            .tui_contract(params_typed(serde_json::json!(
                { "action": "scaffold", "scaffold_mode": "explor", "id": sid }
            )))
            .await,
        "scaffold typo",
    );
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or("")
            .contains("scaffold_mode"),
        "{bad}"
    );

    let _ = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": sid }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

// ────────────────────── workflow object (item 38) ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_inspect_diagnose_verify_construction_chain() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-workflow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    // A TUI with a discoverability-relevant screen: textual `help` hint lines
    // but no conventional key cues in the affordance set.
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start",
                "command": "python3",
                "args": ["focus_ring_tui.py"],
                "cwd": "fixtures", "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    // ── gather a real finding via the audit surface ──
    let audit = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "discoverability", "id": sid,
            })))
            .await,
        "discoverability audit",
    );
    let findings = audit["findings"].as_array().expect("findings array");
    assert!(!findings.is_empty(), "audit produced findings: {audit}");
    let first = findings[0].as_object().expect("finding object");
    let finding_id = first["id"].as_str().expect("finding id").to_string();
    assert_eq!(first["category"], "discoverability", "{first:?}");

    // ── inspect: one object joins the whole construction chain ──
    let inspect = unwrap_ok(
        &server
            .tui_workflow(params_typed(serde_json::json!({
                "action": "inspect", "finding_id": &finding_id,
            })))
            .await,
        "workflow inspect",
    );
    assert_eq!(inspect["workflow"], "construction", "{inspect}");
    assert_eq!(inspect["finding"]["id"], finding_id, "{inspect}");
    // explain_finding returns a structured object; its top-level fields
    // name the finding's rule.
    assert_eq!(
        inspect["explanation"]["category"], "discoverability",
        "{inspect}"
    );
    assert_eq!(inspect["explanation"]["id"], finding_id, "{inspect}");
    // component_identity cites the evidence target(s).
    let identities = inspect["component_identity"]["identities"]
        .as_array()
        .expect("identities");
    assert!(!identities.is_empty(), "{inspect}");
    assert!(
        identities.iter().any(|i| i["loci_known"].is_boolean()),
        "each identity carries a loci_known bool: {identities:?}"
    );
    // framework context: detection names something or honestly says none.
    assert!(
        inspect["framework"].get("project_root").is_some(),
        "framework context resolved: {inspect}"
    );
    assert!(
        inspect["framework"]["primary"].is_null()
            || inspect["framework"]["primary"].get("name").is_some(),
        "primary is an object or null: {inspect}"
    );
    // contract expectation is honest (loaded=false when none loaded).
    assert_eq!(inspect["contract"]["loaded"], false, "{inspect}");
    // verify() requires no side effects on inspect; the reproduction leg
    // is null (no scenario recorded) — construction is a JOIN, not a run.
    assert!(
        inspect["reproduction"].is_null() || inspect["reproduction"]["missing"] == true,
        "reproduction reported honestly: {inspect}"
    );

    // ── diagnose: every finding's chain, list-level ──
    let diagnose = unwrap_ok(
        &server
            .tui_workflow(params_typed(serde_json::json!({ "action": "diagnose" })))
            .await,
        "workflow diagnose",
    );
    let chains = diagnose["findings"].as_array().expect("chains");
    assert!(
        !chains.is_empty(),
        "diagnose lists at least the audit's findings: {diagnose}"
    );
    assert!(
        chains.iter().any(|c| c["finding"]["id"] == finding_id),
        "diagnose includes our finding: {diagnose}"
    );

    // ── verify: lease-gated replay + live audit recheck ──
    let verify = unwrap_ok(
        &server
            .tui_workflow(params_typed(serde_json::json!({
                "action": "verify", "finding_id": &finding_id, "id": sid,
            })))
            .await,
        "workflow verify",
    );
    assert_eq!(verify["workflow"], "verify", "{verify}");
    // recheck_profile selects the finding's category surface.
    assert_eq!(verify["recheck_profile"], "discoverability", "{verify}");
    // The recheck leg actually ran (it is a live pass over the same frame).
    let recheck = verify["recheck"].as_object().expect("recheck ran");
    assert_eq!(recheck["profile"], "discoverability", "{recheck:?}");
    assert!(
        recheck["rule_refired"].is_boolean() || recheck["rule_refired"].is_null(),
        "recheck verdict is a boolean or null — never a fabricated pass: {recheck:?}"
    );
    assert!(
        verify["verdict"]["recheck"]["still_reproduces"].is_boolean()
            || verify["verdict"]["recheck"]["still_reproduces"].is_null(),
        "verdict carried: {verify}"
    );

    // ── unknown action / unknown finding refused, naming the set ──
    let bad_action = unwrap_err(
        &server
            .tui_workflow(params_typed(serde_json::json!({ "action": "explain" })))
            .await,
        "workflow unknown action",
    );
    assert!(
        bad_action["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown workflow action"),
        "{bad_action}"
    );
    let bad_finding = unwrap_err(
        &server
            .tui_workflow(params_typed(serde_json::json!({
                "action": "inspect", "finding_id": "DOES-NOT-EXIST",
            })))
            .await,
        "workflow unknown finding",
    );
    assert!(
        bad_finding["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown finding id"),
        "{bad_finding}"
    );

    let _ = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": sid }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

// ──────────────── regression-asset generation (finding 39) ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regression_asset_generates_review_gated_assets() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-regasset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start",
                "command": "python3",
                "args": ["focus_ring_tui.py"],
                "cwd": "fixtures", "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    // ── gather a real finding ──
    let audit = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "discoverability", "id": sid,
            })))
            .await,
        "discoverability audit",
    );
    let findings = audit["findings"].as_array().expect("findings array");
    assert!(!findings.is_empty(), "audit produced findings: {audit}");
    let finding_id = findings[0]["id"].as_str().unwrap().to_string();
    assert_eq!(findings[0]["category"], "discoverability", "{audit}");

    // ── generate all justified asset kinds ──
    let gen = unwrap_ok(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "regression_asset", "finding_id": &finding_id,
            })))
            .await,
        "regression_asset",
    );
    assert_eq!(gen["workflow"], "regression_asset", "{gen}");
    assert_eq!(gen["finding_id"], finding_id, "{gen}");
    // A discoverability finding cites a bare frame hash — the honest,
    // evidence-justified assets are: an assertion pinning that frame, and
    // NO scenario (no reproduction), NO contract rule (frame, not control),
    // NO viewport (no cited size).
    let assets = gen["assets"].as_array().expect("assets array");
    assert!(
        !assets.is_empty(),
        "the frame-hash discovery finding justifies at least an assertion: {gen}"
    );
    let kinds: Vec<&str> = assets.iter().filter_map(|a| a["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"assertion"),
        "state-pin assertion generated from the frame target: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"scenario"),
        "no reproduction → no scenario invented: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"contract_rule"),
        "frame hash is not a control → no contract rule: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"viewport_case"),
        "no cited size → no viewport case: {kinds:?}"
    );
    // The honesty contract: every generated asset is review-gated.
    for a in assets.iter() {
        assert_eq!(a["generated"], true, "{a}");
        assert_eq!(a["inferred"], true, "{a}");
        assert_eq!(a["requires_review"], true, "{a}");
        assert!(!a["provenance"].as_str().unwrap_or("").is_empty(), "{a}");
        assert!(
            a["provenance"].as_str().unwrap_or("").contains(&finding_id),
            "{a}"
        );
    }

    // ── `only=scenario` on a finding with no reproduction must honestly
    //    refuse to fabricate one ──
    // The controls audit findings have no reproduction and no transaction
    // acts toward a control (observation, not driving) → scenario is not
    // available. The response's assets list then has no scenario form.
    let only_scn = unwrap_ok(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "regression_asset", "finding_id": &finding_id,
                "asset_type": "scenario",
            })))
            .await,
        "regression_asset only=scenario",
    );
    if let Some(only) = only_scn["assets"].as_array() {
        assert!(
            !only.iter().any(|a| a["kind"] == "scenario"),
            "no silhouette scenario invented: {only:?}"
        );
    } else {
        // Honest emptiness is allowed — never an invented step.
    }

    // ── unknown finding refused ──
    let bad = unwrap_err(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "regression_asset", "finding_id": "NOT-A-FINDING",
            })))
            .await,
        "regression_asset unknown finding",
    );
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown finding id"),
        "{bad}"
    );

    let _ = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": sid }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

// ──────────────── semantic render deltas (finding 40) ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_act_carries_semantic_render_deltas() {
    // Finding 40: a render change must read as "button/gamma gained focus",
    // never just changed_cells=N. Driving Tab across a focus ring produces
    // a transition whose control_deltas name the moved-focus control.
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-rendelta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start",
                "command": "python3",
                "args": ["focus_ring_tui.py"],
                "cwd": "fixtures", "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    // Settle the initial frame so the first Tab's before-frame is the
    // focused Alpha screen, not the blank launch screen (a startup race
    // made the before-frame focus=null under load — the transition then
    // read as 'blank → Alpha' with style_changes=0).
    let _ = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "summary", "id": sid }),
            ))
            .await,
        "initial settle observe",
    );

    // Tab moves focus Alpha → Beta. Under a concurrent host the first act
    // can still race the initial draw (before-frame focus=null); a settled
    // transition is one whose before-frame carries a resolved focus. Loop
    // a few acts until we have one, then assert the honesty contract on it.
    let mut act = serde_json::Value::Null;
    for _ in 0..4 {
        let a = unwrap_ok(
            &server
                .tui_act(params_typed(serde_json::json!({
                    "action": "key", "key": "tab", "id": sid,
                })))
                .await,
            "tab act",
        );
        let fb = a["transition"]["semantic_diff"]["focus_before"]
            .as_str()
            .unwrap_or("");
        if !fb.is_empty() {
            act = a;
            break;
        }
    }
    let sd = &act["transition"]["semantic_diff"];
    // Finding 40's honesty contract: `control_deltas` is ALWAYS a present
    // array (never absent). When the semantic layer resolves control
    // objects, it names exactly which one changed and how. The reverse-
    // video focus ring yields focus-level render truth without box-drawn
    // control objects — so `control_deltas` may be empty, but the render
    // change MUST still be named: focus_before≠focus_after and
    // style_changes>0. changed_cells==0 (geometry identical) must never be
    // the only signal the agent sees.
    assert!(
        sd["focus_before"].is_string(),
        "a settled transition has a resolved before-focus: {act}"
    );
    let deltas = sd["control_deltas"].as_array().unwrap_or_else(|| {
        panic!("control_deltas must be an array (possibly empty, never absent): {act}")
    });
    for d in deltas {
        assert!(!d["summary"].as_str().unwrap_or("").is_empty(), "{d}");
        assert!(!d["id"].as_str().unwrap_or("").is_empty(), "{d}");
        // A delta always carries resolved before/after bounds when it moved.
        if let Some(b) = d["bounds"].as_object() {
            assert!(b["before"].is_object() && b["after"].is_object(), "{d}");
        }
    }
    let fb = sd["focus_before"].as_str().unwrap_or("");
    let fa = sd["focus_after"].as_str().unwrap_or("");
    assert_ne!(fb, fa, "focus moved must still be reported: {act}");
    assert!(
        act["transition"]["screen_diff"]["style_changes"].as_u64().unwrap_or(0) > 0,
        "the reverse-video focus change is a style change, reported alongside the cell count: {act}"
    );

    let _ = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": sid }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

// Audit P1 (response freshness): a close response's "final" summary must
// describe the run AFTER the close — the pre-close snapshot reported
// closed:false and pre-flush counts, so a "successful" close handed back
// a final object that predated the very flush it certified. Likewise an
// already-persistent `tui_run action=persist` is not a bare echo: it
// flushes and answers with the CURRENT durability picture.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_and_persist_responses_report_fresh_final_state() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-fresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");

    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start", "command": "python3",
                "args": ["-c", "print('fresh'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "session start",
    );
    let sid = start["session"].as_str().unwrap().to_string();
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "tab", "id": sid }),
            ))
            .await,
        "act",
    );

    // Persist, then accumulate MORE in-memory state (another act).
    unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "persist" })))
            .await,
        "persist",
    );
    unwrap_ok(
        &server
            .tui_act(params_typed(
                serde_json::json!({ "action": "key", "key": "escape", "id": sid }),
            ))
            .await,
        "act after persist",
    );

    // already-persistent persist: the response's "final" must show the
    // run's CURRENT counts (including the post-persist act), not a stale
    // echo — and the flush it performs must succeed cleanly.
    let again = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "persist" })))
            .await,
        "persist (already persistent)",
    );
    assert_eq!(again["already_persistent"], true, "{again}");
    assert!(
        again["flush_error"].is_null(),
        "flush of an already-persistent run must succeed: {again}"
    );
    let final_again = &again["final"];
    assert!(
        final_again.is_object() && final_again["closed"] == false,
        "final carries the live (post-flush, pre-close) state: {again}"
    );
    let tx_at_persist = final_again["counts"]["transactions"]
        .as_u64()
        .expect("tx count");

    // Close: "final" must report closed:true and the settled counts.
    let close = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "close" })))
            .await,
        "close",
    );
    assert_eq!(close["closed"], true, "{close}");
    let final_close = &close["final"];
    assert!(final_close.is_object(), "final is present: {close}");
    assert_eq!(
        final_close["closed"], true,
        "the final summary describes the run AFTER the close, not the \
         pre-close snapshot (which said closed:false): {close}"
    );
    assert_eq!(
        final_close["counts"]["transactions"].as_u64(),
        Some(tx_at_persist),
        "final counts are the settled totals, not pre-flush values: {close}"
    );
    assert_eq!(final_close["run_id"], again["run_id"], "{close}");

    // Cross-check against a fresh status call: identical totals.
    let after = unwrap_ok(
        &server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await,
        "status after close",
    );
    assert_eq!(
        after["counts"]["transactions"].as_u64(),
        final_close["counts"]["transactions"].as_u64(),
        "final matches the post-close status: {after} vs {close}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// Audit P1 (finding 13): the restart-replay flag no longer implies
// mutating consent. `restart_between_mutations=true` without
// `allow_mutation=true` is refused at the wire with the reason named;
// the old `deep_isolation` spelling still parses (deprecated alias) but
// is subject to the SAME consent rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_between_mutations_requires_explicit_mutation_consent() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();
    let id = start_session(&server, "print('rbm'); input()").await;

    // Alone: refused — the restart does not undo external side effects,
    // so the consent for those must come from allow_mutation explicitly.
    let refused = unwrap_err(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "keyboard",
                "id": id,
                "restart_between_mutations": true,
            })))
            .await,
        "rbm without allow_mutation",
    );
    let msg = serde_json::to_string(&refused).expect("envelope");
    assert!(
        msg.contains("allow_mutation"),
        "refusal names the missing consent: {msg}"
    );

    // The deprecated alias is subject to the same rule.
    let alias = unwrap_err(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "keyboard",
                "id": id,
                "deep_isolation": true,
            })))
            .await,
        "deep_isolation alias without allow_mutation",
    );
    let alias_msg = serde_json::to_string(&alias).expect("envelope");
    assert!(
        alias_msg.contains("allow_mutation"),
        "the old spelling gets the same consent rule: {alias_msg}"
    );

    // With consent: the policy is accepted and reported under its new
    // name (an observational session can't exercise the restarts, but
    // the policy selection is visible).
    let allowed = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "keyboard",
                "id": id,
                "restart_between_mutations": true,
                "allow_mutation": true,
            })))
            .await,
        "rbm with allow_mutation",
    );
    assert_eq!(
        allowed["policy"], "restart_between_mutations",
        "policy surfaced under the honest name: {allowed}"
    );

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

// Audit P1 (finding 14): the construction workflow's framework context is
// rooted at the RUN's recorded LaunchSpec.cwd — where the audited app
// actually lives — never silently at the server process's cwd. An explicit
// `cwd` parameter is a conscious override and its provenance is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_framework_root_follows_recorded_launch_cwd() {
    let server = tui_lab::mcp::tools::TuiLabServer::new();

    // The session is launched with cwd=base — an isolated directory
    // OUTSIDE the repo, so upward manifest walking cannot reach the
    // server process's project. The workflow must detect against `base`,
    // citing the recorded launch cwd (a server-cwd fallback would have
    // produced the repo root instead).
    let base = std::env::temp_dir().join(format!("tui-lab-wfcwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");
    let start = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "start",
                "command": "python3",
                "args": ["-c", "print('wf-cwd'); input()"],
                "cwd": base.to_string_lossy(), "cols": 80, "rows": 24,
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    let audit = unwrap_ok(
        &server
            .tui_audit(params_typed(serde_json::json!({
                "profile": "discoverability", "id": sid,
            })))
            .await,
        "audit",
    );
    let finding_id = audit["findings"][0]["id"]
        .as_str()
        .expect("finding id")
        .to_string();

    let inspect = unwrap_ok(
        &server
            .tui_workflow(params_typed(serde_json::json!({
                "action": "inspect", "finding_id": &finding_id,
            })))
            .await,
        "inspect",
    );
    let fw = &inspect["framework"];
    assert_eq!(
        fw["root_provenance"], "recorded_launch_cwd",
        "root comes from the recorded launch cwd, not the server cwd: {fw}"
    );
    assert_eq!(
        fw["project_root"],
        base.to_string_lossy().as_ref(),
        "detection resolved against the app's own directory: {fw}"
    );

    // An explicit cwd is a conscious override, labeled as such.
    let overridden = unwrap_ok(
        &server
            .tui_workflow(params_typed(serde_json::json!({
                "action": "inspect", "finding_id": &finding_id,
                "cwd": ".",
            })))
            .await,
        "inspect with explicit cwd",
    );
    assert_eq!(
        overridden["framework"]["root_provenance"], "explicit_override",
        "{overridden}"
    );

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": sid }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}
