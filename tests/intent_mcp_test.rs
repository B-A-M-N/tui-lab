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
