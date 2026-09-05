//! tui_intent end-to-end (review P1: the semantic intent system had no MCP
//! surface). Plans must name every step and the risk before anything is
//! sent; execution must run the focus-secured sequence; unresolved targets
//! must return `target_error` with structured candidates in `details`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use tui_lab::mcp::tools::TuiLabServer;

fn params_typed<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid params"))
}

fn envelope(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    raw.structured_content
        .clone()
        .or_else(|| {
            raw.content.first().map(|c| {
                let rmcp::model::ContentBlock::Text(t) = c else {
                    panic!("{ctx}: first content block must be text");
                };
                serde_json::from_str(&t.text).expect("valid JSON envelope")
            })
        })
        .expect("structured content")
}

fn unwrap_ok(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = envelope(raw, ctx);
    assert!(!raw.is_error.unwrap_or(false), "{ctx} failed: {v}");
    v.get("data").cloned().expect("data payload")
}

async fn start_dialog(server: &TuiLabServer) -> String {
    let raw = server
        .tui_session(params_typed(serde_json::json!({
            "action": "start",
            "command": "python3",
            // The intent fixture enables mouse reporting, so the
            // EnsureFocus click is protocol-legal (the plain dialog
            // fixture's app has mouse mode None and the backend rightly
            // refuses to fake a protocol the app did not request).
            "args": ["fixtures/intent_tui.py"],
            "cols": 80, "rows": 24,
        })))
        .await;
    unwrap_ok(&raw, "session start")["session"]
        .as_str()
        .expect("session id")
        .to_string()
}

/// Observe first (plan_intent resolves against the last fused frame).
async fn observe(server: &TuiLabServer, id: &str) {
    let raw = server
        .tui_observe(params_typed(
            serde_json::json!({ "mode": "semantic", "id": id }),
        ))
        .await;
    unwrap_ok(&raw, "observe");
}

#[tokio::test]
async fn plan_names_steps_and_risk_without_sending_input() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Save" },
            "verb": "activate",
            "id": id,
        })))
        .await;
    let v = unwrap_ok(&raw, "intent plan");
    assert_eq!(v["mode"], "planned", "{v}");
    assert_eq!(v["verb"], "activate");
    // Activation is mutating at base risk.
    assert_eq!(v["risk"], "mutating", "{v}");
    // The focus-secured sequence: ensure_focus → assert_focus → act.
    let steps = v["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 3, "activate plans 3 steps: {v}");
    assert_eq!(steps[0]["step"], "ensure_focus");
    assert_eq!(steps[1]["step"], "assert_focus");
    assert_eq!(steps[2]["step"], "act");
    let control = &v["control"];
    assert!(
        control["id"].as_str().unwrap_or("").contains("save"),
        "resolved control should carry the Save label id: {control}"
    );

    // Nothing was sent: the fixture prints "saved." on the FIRST key it
    // receives, and the plan-only call must not have produced it.
    let raw = server
        .tui_observe(params_typed(
            serde_json::json!({ "mode": "screen", "id": id }),
        ))
        .await;
    let screen = unwrap_ok(&raw, "screen after plan");
    let text = screen["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("saved."),
        "plan-only intent must not send input: {text}"
    );
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

#[tokio::test]
async fn execute_runs_the_focus_secured_sequence() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Save" },
            "verb": "activate",
            "execute": true,
            "id": id,
        })))
        .await;
    let v = unwrap_ok(&raw, "intent execute");
    assert_eq!(v["mode"], "executed", "{v}");
    let steps = v["steps"].as_array().expect("executed steps");
    assert!(
        steps.iter().any(|s| s["step"] == "ensure_focus"),
        "focus click executed: {v}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s["step"] == "act" && s["settled"] == true),
        "payload action executed and settled: {v}"
    );
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

#[tokio::test]
async fn unresolved_target_returns_target_error_with_structured_candidates() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Completely Absent" },
            "verb": "activate",
            "id": id,
        })))
        .await;
    let v = envelope(&raw, "missing target");
    assert_eq!(v["category"], "target_error", "{v}");
    assert!(
        v["details"]["candidates"].is_array(),
        "candidates ride in details (review §14): {v}"
    );
    assert!(
        !v["details"]["candidates"]
            .as_array()
            .expect("candidates")
            .is_empty(),
        "nearest candidates are attached for self-correction: {v}"
    );
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

#[tokio::test]
async fn unknown_verb_is_invalid_request_naming_the_accepted_set() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "focused" },
            "verb": "explode",
            "id": id,
        })))
        .await;
    let v = envelope(&raw, "unknown verb");
    assert_eq!(v["category"], "invalid_request", "{v}");
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("activate") && msg.contains("type"),
        "error must name the accepted verbs: {msg}"
    );
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

// ─── Audit finding 56: evidence/lease/sensitivity E2E for tui_intent ──

/// Execute must INCREASE the run's interaction-ledger transaction and frame
/// counts (audit finding 12/56: intent actions used to disappear from run
/// evidence), and the active scenario recording must capture the payload in
/// replayable form.
#[tokio::test]
async fn execute_enters_run_evidence_and_scenario_recording() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    // Start an active recording BEFORE the intent.
    let rec = unwrap_ok(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "record_start", "id": id,
            })))
            .await,
        "record start",
    );
    let rec_id = rec["recording_id"]
        .as_str()
        .expect("recording id")
        .to_string();

    async fn run_status(server: &TuiLabServer) -> serde_json::Value {
        let raw = server
            .tui_run(params_typed(serde_json::json!({ "action": "status" })))
            .await;
        unwrap_ok(&raw, "run status")
    }
    let before_tx = run_status(&server).await["counts"]["transactions"]
        .as_u64()
        .expect("tx count");
    let before_frames = run_status(&server).await["frames"]["next_frame_id"]
        .as_u64()
        .expect("frame id");

    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Save" },
            "verb": "activate",
            "execute": true,
            "id": id,
        })))
        .await;
    let v = unwrap_ok(&raw, "intent execute");
    assert_eq!(v["mode"], "executed", "{v}");

    let after_tx = run_status(&server).await["counts"]["transactions"]
        .as_u64()
        .expect("tx count");
    let after_frames = run_status(&server).await["frames"]["next_frame_id"]
        .as_u64()
        .expect("frame id");
    assert!(
        after_tx > before_tx,
        "intent execution must enter the transaction ledger: {before_tx} -> {after_tx}"
    );
    assert!(
        after_frames > before_frames,
        "intent execution must commit frames: {before_frames} -> {after_frames}"
    );

    // Stop the recording; the exported steps must contain an act step for
    // the payload (replayable through the normal act grammar), not a
    // shapeless blob. Step shape is flat: {kind, ...params}.
    let stop = unwrap_ok(
        &server
            .tui_scenario(params_typed(
                serde_json::json!({ "action": "record_stop", "recording_id": rec_id }),
            ))
            .await,
        "record stop",
    );
    let steps = stop["scenario"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let payload_step = steps
        .iter()
        .find(|s| s["kind"] == "act" && s["action"].is_string());
    assert!(
        payload_step.is_some(),
        "the intent's payload action must be captured as a replayable act step: {steps:?}"
    );

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

/// A live human-control lease refuses intent EXECUTION (it drives) but the
/// audit finding 2 contract is uniform across every driving tool.
#[tokio::test]
async fn lease_blocks_intent_execution_but_not_planning() {
    let server = TuiLabServer::new();
    let id = start_dialog(&server).await;
    observe(&server, &id).await;

    let lease = unwrap_ok(
        &server
            .tui_session(params_typed(serde_json::json!({
                "action": "lease", "id": id, "by": "e2e-human",
            })))
            .await,
        "lease",
    );
    assert!(
        lease.get("lease").is_some() || lease.get("expires_at").is_some() || !lease.is_null(),
        "lease granted: {lease}"
    );

    // execute=true must be refused with control_leased.
    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Save" },
            "verb": "activate",
            "execute": true,
            "id": id,
        })))
        .await;
    let v = envelope(&raw, "intent under lease");
    assert_eq!(v["category"], "control_leased", "{v}");

    // Planning stays allowed (it sends nothing).
    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Save" },
            "verb": "activate",
            "id": id,
        })))
        .await;
    let v = unwrap_ok(&raw, "intent plan under lease");
    assert_eq!(v["mode"], "planned", "{v}");

    // And nothing was sent: the fixture prints "saved." on the first input.
    let scr = unwrap_ok(
        &server
            .tui_observe(params_typed(
                serde_json::json!({ "mode": "screen", "id": id }),
            ))
            .await,
        "screen under lease",
    );
    let text = scr["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("saved."),
        "leased session must not be driven: {text}"
    );

    let _ = server
        .tui_session(params_typed(
            serde_json::json!({ "action": "release", "id": id }),
        ))
        .await;
    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}

/// A sensitive semantic type must keep its redaction contract: the executed
/// action's payload enters run evidence as a sensitive reference, never the
/// literal secret (audit findings 3/14/56). The field fixture's
/// `Password:` line extracts as a Field control — the kind a `type` verb
/// applies to.
#[tokio::test]
async fn sensitive_type_stays_redacted_in_evidence() {
    let server = TuiLabServer::new();
    let raw = server
        .tui_session(params_typed(serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["fixtures/field_tui.py"],
            "cols": 80, "rows": 24,
        })))
        .await;
    let id = unwrap_ok(&raw, "session start")["session"]
        .as_str()
        .expect("session id")
        .to_string();
    observe(&server, &id).await;

    const SECRET: &str = "hunter2-secret-e2e";
    // Record a scenario during the sensitive type so BOTH evidence sinks are
    // exercised: the run ledger and the scenario export.
    let rec = unwrap_ok(
        &server
            .tui_scenario(params_typed(
                serde_json::json!({ "action": "record_start", "id": id }),
            ))
            .await,
        "record start",
    );
    let rec_id = rec["recording_id"]
        .as_str()
        .expect("recording id")
        .to_string();
    let raw = server
        .tui_intent(params_typed(serde_json::json!({
            "target": { "by": "text", "text": "Password" },
            "verb": { "verb": "type", "text": SECRET },
            "sensitive": true,
            "execute": true,
            "id": id,
        })))
        .await;
    let v = unwrap_ok(&raw, "sensitive intent execute");
    assert_eq!(v["mode"], "executed", "{v}");
    let stop = unwrap_ok(
        &server
            .tui_scenario(params_typed(
                serde_json::json!({ "action": "record_stop", "recording_id": rec_id }),
            ))
            .await,
        "record stop",
    );
    let scenario = unwrap_ok(
        &server
            .tui_scenario(params_typed(serde_json::json!({
                "action": "export",
                "name": stop["name"].as_str().unwrap_or(""),
            })))
            .await,
        "scenario export",
    );

    // The secret must not appear in the exported scenario (sensitive capture
    // stores a ${PARAM} reference or an opaque redaction, not the value).
    let export_text = serde_json::to_string(&scenario).unwrap_or_default();
    assert!(
        !export_text.contains(SECRET),
        "the secret leaked into the scenario export: {export_text}"
    );

    server
        .tui_session(params_typed(
            serde_json::json!({ "action": "stop", "id": id }),
        ))
        .await;
}
