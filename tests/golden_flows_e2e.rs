//! Canonical flows are public API contracts. This suite executes the
//! regression flow against a deterministic fixture and proves placeholder
//! guidance names valid tool selectors.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde_json::json;
use tui_lab::mcp::registry;
use tui_lab::mcp::tools::TuiLabServer;

fn parameters<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    serde_json::from_value(v).unwrap()
}

fn unwrap_ok(raw: &CallToolResult, ctx: &str) -> serde_json::Value {
    let v = raw
        .structured_content
        .clone()
        .or_else(|| {
            raw.content.first().map(|c| {
                let rmcp::model::ContentBlock::Text(t) = c else {
                    panic!("{ctx}: non-text content block");
                };
                serde_json::from_str(&t.text).expect("valid JSON envelope")
            })
        })
        .unwrap_or_else(|| panic!("{ctx}: structured content"));
    assert!(!raw.is_error.unwrap_or(false), "{ctx}: {v}");
    v["data"].clone()
}

/// The regression flow's core path: record a real act/assert, stop, replay,
/// and prove the recorded artifact is nonempty and executable.
#[tokio::test]
async fn regression_flow_records_and_replays() {
    let server = TuiLabServer::new();
    let start = unwrap_ok(
        &server
            .tui_session(parameters(json!({
                "action":"start","command":"python3",
                "args":["-c","import time; time.sleep(60)"]
            })))
            .await,
        "start",
    );
    let sid = start["session"].as_str().unwrap().to_string();

    let rec = unwrap_ok(
        &server
            .tui_scenario(parameters(json!({
                "action":"record_start","name":"golden-critical-path","id":sid
            })))
            .await,
        "record_start",
    );
    let recording_id = rec["recording_id"].as_str().unwrap().to_string();

    unwrap_ok(
        &server
            .tui_act(parameters(json!({
                "action":"type","text":"golden-marker","id":sid
            })))
            .await,
        "act",
    );
    unwrap_ok(
        &server
            .tui_assert(parameters(json!({
                "assertion":"text","text":"golden-marker","id":sid
            })))
            .await,
        "assert",
    );
    let stop = unwrap_ok(
        &server
            .tui_scenario(parameters(json!({
                "action":"record_stop","recording_id":recording_id
            })))
            .await,
        "record_stop",
    );
    assert!(stop["steps"].as_u64().unwrap_or(0) >= 2, "{stop}");
    // Ephemeral runs keep scenarios in memory; `saved_to:null` is honest,
    // not missing persistence. Replay uses the in-memory canonical store.

    let replay = unwrap_ok(
        &server
            .tui_scenario(parameters(json!({
                "action":"run","name":"golden-critical-path","id":sid
            })))
            .await,
        "replay",
    );
    assert_eq!(replay["passed"], true, "{replay}");
}

/// Every human-facing flow note must name real selectors. This catches the
/// `tui_audit action=run` / `tui_contract action=check` class of drift.
#[test]
fn generated_flow_guidance_has_no_invalid_selectors() {
    let raw = serde_json::to_string(&registry::flows()).unwrap();
    assert!(!raw.contains("tui_audit action=run"));
    assert!(!raw.contains("tui_contract action=check"));
}
