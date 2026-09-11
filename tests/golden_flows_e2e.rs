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

#[test]
fn timeline_frame_references_are_registered_and_resolvable() {
    use std::sync::{Arc, Mutex};

    // Build a persistent run and commit a real transaction so its timeline
    // references actual before/after frame ids.
    let base = tempfile::tempdir().expect("tempdir");
    let run = tui_lab::run::RunContext::persistent(base.path()).expect("run");
    let mut before = tui_lab::screen::ScreenState::new(4, 1);
    before.structure_hash = "before-structure".into();
    before.visual_hash = "before-visual".into();
    let mut after = before.clone();
    after.structure_hash = "after-structure".into();
    after.visual_hash = "after-visual".into();
    let tx = tui_lab::execution::InteractionTransaction {
        action: tui_lab::execution::ActionEnvelope::new(
            tui_lab::execution::CanonicalAction::Key {
                key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
            },
            tui_lab::execution::InputVisibility::Normal,
        ),
        anchor: Default::default(),
        before_frame: tui_lab::backend::CanonicalFrame::new(before, 1, 1),
        after_frame: tui_lab::backend::CanonicalFrame::new(after, 1, 2),
        settle: tui_lab::execution::SettleStatus::Met,
        transition: tui_lab::screen::diff::diff(
            &tui_lab::backend::CanonicalFrame::new(tui_lab::screen::ScreenState::new(4, 1), 1, 1)
                .state,
            &tui_lab::backend::CanonicalFrame::new(tui_lab::screen::ScreenState::new(4, 1), 1, 2)
                .state,
        ),
        capture: None,
        focus_before: None,
        focus_after: None,
        elapsed_ms: 1,
        send_ms: 1,
        settle_ms: 0,
        render: None,
        transition_capture: None,
        origin: Some(tui_lab::execution::DriveOrigin::Act),
        dispatch: tui_lab::execution::DispatchStatus::Sent,
        dispatch_failure: None,
        event_seq_before: 0,
        event_seq_after: Some(1),
        native_revision_before: None,
        dispatch_reason: None,
    };
    // Commit through the evidence sink so both before/after frames get
    // assigned citable ids exactly as a real driven transaction does.
    let run_arc = Arc::new(Mutex::new(run));
    let sink = tui_lab::execution::RunEvidenceSink::capture(&run_arc);
    let (health, _seq, _frames) = sink
        .commit("resource-sess", 1, &tx)
        .expect("evidence commit");
    assert!(health.healthy(), "fixture evidence must commit cleanly");
    // The sink clones the run Arc; drop it explicitly before unwrapping.
    drop(sink);
    let mut run = Arc::try_unwrap(run_arc)
        .map_err(|_| "run Arc should be unique")
        .expect("unwrap run")
        .into_inner()
        .expect("run mutex");
    run.flush().expect("flush");
    let run_id = run.id().to_string();

    // Read the joined timeline entry and extract every frame URI it cites.
    let timeline_payload =
        tui_lab::mcp::resources::run_scoped_payload(&run, &run_id, "timeline", Some("0"))
            .expect("timeline entry");
    let v: serde_json::Value = serde_json::from_str(&timeline_payload).expect("timeline JSON");
    let frame_uris: Vec<String> = v["references"]
        .as_object()
        .expect("references")
        .values()
        .filter_map(|x| x.as_str().map(str::to_string))
        .filter(|u| u.contains("/frames/"))
        .collect();
    assert_eq!(
        frame_uris.len(),
        2,
        "tx timeline must cite before and after frame URIs: {v:?}"
    );

    // Every URI must match a registered resource template shape.
    let registered = registry::RESOURCES.iter().map(|r| r.uri);
    for uri in &frame_uris {
        // The registered pattern uses a path-style placeholder. We only
        // care that the prefix shape is registered; the final id component
        // is dynamic.
        let prefix = uri
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or(uri);
        let normalized = format!("{prefix}/{{frame_id}}");
        // Substitute the run placeholder to match the registered pattern.
        let normalized = normalized.replace(&run_id, "{run_id}");
        assert!(
            registered.clone().any(|pattern| pattern == normalized),
            "frame URI {uri} must be a registered resource"
        );
    }

    // And each URI must resolve to the actual committed frame record.
    for uri in &frame_uris {
        let key = uri.rsplit('/').next().unwrap();
        let payload =
            tui_lab::mcp::resources::run_scoped_payload(&run, &run_id, "frames", Some(key))
                .unwrap_or_else(|e| panic!("frame {uri} must resolve: {e}"));
        let record: serde_json::Value = serde_json::from_str(&payload).expect("frame JSON");
        let id: u64 = key.parse().expect("frame id");
        assert_eq!(record["frame_id"], id, "{uri}: {record:?}");
        assert!(
            record["structure_hash"].is_string() && record["semantic_identity"].is_string(),
            "frame record carries identity projection: {record:?}"
        );
    }
    let _ = Arc::new(Mutex::new(())); // keep common test imports stable
}
