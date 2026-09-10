//! tui_scenario / tui_record: scenario replay and recording/capture.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_record` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_record(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiRecordParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::RecordFormat as RF;
    let fmt = p
        .format
        .clone()
        .unwrap_or(crate::mcp::params::Known::Known(RF::Start));
    let Some(rfmt) = (match &fmt {
        crate::mcp::params::Known::Known(f) => Some(*f),
        crate::mcp::params::Known::Other(o) => {
            return err(
                    ErrorCategory::Unsupported,
                    format!(
                        "recording format '{}' is not implemented by the current backend; available: start/stop (.cast lifecycle), svg, png (one-shot captures)",
                        o
                    ),
                );
        }
    }) else {
        unreachable!()
    };
    let selector = p.id.clone();
    let run = s.run.clone();
    s.with_sess(selector.as_deref(), move |sess| {
            match rfmt {
            // Attach the raw PTY hook (audit item 24): every byte the reader
            // thread sees from now on is captured with timing.
            RF::Start => {
                // Wave G item 76: starting a recording while a human drives
                // risks capturing their keystrokes; the lifecycle refuses
                // under a live lease (one-shot captures stay allowed).
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
                sess.enable_recording(false);
                let (boundary, fidelity, lossy) = match sess.backend_kind {
                    crate::session::state::BackendKind::PortableVt
                    | crate::session::state::BackendKind::PtyLine => (
                        "pty-bytes",
                        "raw_pty_stream",
                        false,
                    ),
                    crate::session::state::BackendKind::Pipe => (
                        "separated-streams",
                        "separated_streams",
                        false,
                    ),
                    crate::session::state::BackendKind::TmuxAttach => (
                        "rendered-pane-snapshots",
                        "rendered_snapshots",
                        true,
                    ),
                };
                let note = if lossy {
                    "output is reconstructed from sampled pane snapshots; it is not a raw byte stream and cannot prove protocol timing".to_string()
                } else if boundary == "separated-streams" {
                    "stdout and stderr are captured at their stream boundaries; call format=stop to flush to a .cast file".to_string()
                } else {
                    "output is captured at the raw PTY byte boundary; call format=stop to flush to a .cast file".to_string()
                };
                ok(json!({
                    "recording": "started",
                    "boundary": boundary,
                    "fidelity": fidelity,
                    "lossy": lossy,
                    "note": note
                }))
            }
            // Detach + write the .cast into the run's recordings dir.
            RF::Stop => {
                // Wave G item 76: same lease rule as record start — the
                // lifecycle is machine-driving coordination, not observation.
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
                // stop_recording() detaches the hook and hands back the sink
                // (disable_recording() would drop it before retrieval).
                let rec = match sess.stop_recording() {
                    Some(r) => r,
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            "no recording in progress (call format=start first)",
                        )
                    }
                };
                let (events, ndjson) = {
                    let r = rec.lock().expect("recorder");
                    (r.event_count(), r.to_ndjson())
                };
                let body = ndjson.join("\n") + "\n";
                let file_name = format!(
                    "{}-{}.cast",
                    sess.id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                );
                // Persistent run: write straight into the artifact root.
                // Ephemeral run: retain in run memory so a later `tui_run
                // persist` carries the recording into the durable root
                // (goal spec: promotion preserves "recordings already held
                // in memory") — the content is also returned inline.
                let size = body.len() as u64;
                let (path, artifact) = {
                    let mut run = run.lock().unwrap();
                    match run.run_dir().cloned() {
                        Some(dir) => {
                            let rec_dir = dir.join("recordings");
                            let _ = std::fs::create_dir_all(&rec_dir);
                            let file = rec_dir.join(&file_name);
                            match std::fs::write(&file, &body) {
                                Ok(()) => {
                                    // Typed ref for the persisted artifact (Wave B item 15).
                                    let rel = std::path::PathBuf::from("recordings")
                                        .join(&file_name);
                                    let r = run.register_artifact(
                                        crate::run::ArtifactKind::Recording,
                                        Some(rel),
                                        Some(size),
                                        Some(sess.id.clone()),
                                        format!("pty recording, {} events", events),
                                    )
                                    .ok();
                                    (Some(file.to_string_lossy().to_string()), r)
                                }
                                Err(e) => {
                                    return err(
                                        ErrorCategory::BackendError,
                                        format!("recording flush failed: {}", e),
                                    )
                                }
                            }
                        }
                        None => {
                            run.hold_recording(file_name, body.clone());
                            // Ephemeral: registered without a path; the ref
                            // resolves once the run is promoted.
                            let r = run.register_artifact(
                                crate::run::ArtifactKind::Recording,
                                None,
                                Some(size),
                                Some(sess.id.clone()),
                                format!("pty recording, {} events (held, ephemeral run)", events),
                            )
                            .ok();
                            (None, r)
                        }
                    }
                };
                ok(json!({
                    "recording": "stopped",
                    "events": events,
                    "saved_to": path,
                    "artifact": artifact.as_ref().map(|a| serde_json::json!({
                        "id": a.id,
                        "kind": a.kind,
                        "path": a.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                        "size": a.size,
                        "summary": a.summary,
                    })),
                    "held_in_run": path.is_none(),
                    "note": path.is_none().then(|| "ephemeral run: held in run memory; tui_run persist will write it to the durable root".to_string()),
                    "inline_events": path.is_none().then_some(ndjson),
                }))
            }
            RF::Cast => err(
                ErrorCategory::InvalidRequest,
                "format='cast' is not a lifecycle action; use format=start then format=stop (produces asciinema v3 .cast)",
            ),
            // Wave F item 57: one-shot screen captures for human debugging.
            // SVG is the faithful render (styled runs + cursor); PNG is the
            // raster fallback (ASCII glyphs + block degradation for other
            // scripts — honest about that in the response).
            fmt @ (RF::Svg | RF::Png) => {
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let is_svg = fmt == RF::Svg;
                let (body, ext) = if is_svg {
                    (crate::screen::capture::to_svg(&screen).into_bytes(), "svg")
                } else {
                    (crate::screen::capture::to_png(&screen), "png")
                };
                let file_name = format!(
                    "{}-{}.{}",
                    sess.id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                    ext,
                );
                let size = body.len() as u64;
                let (path, artifact) = {
                    let mut run = run.lock().unwrap();
                    let kind = crate::run::ArtifactKind::Capture;
                    match run.run_dir().cloned() {
                        Some(dir) => {
                            let cap_dir = dir.join("captures");
                            let _ = std::fs::create_dir_all(&cap_dir);
                            let file = cap_dir.join(&file_name);
                            match std::fs::write(&file, &body) {
                                Ok(()) => {
                                    let rel =
                                        std::path::PathBuf::from("captures").join(&file_name);
                                    let r = run.register_artifact(
                                        kind,
                                        Some(rel),
                                        Some(size),
                                        Some(sess.id.clone()),
                                        format!(
                                            "screen capture {}x{} ({} format)",
                                            screen.cols, screen.rows, ext
                                        ),
                                    )
                                    .ok();
                                    (Some(file.to_string_lossy().to_string()), r)
                                }
                                Err(e) => {
                                    return err(
                                        ErrorCategory::BackendError,
                                        format!("capture write failed: {e}"),
                                    )
                                }
                            }
                        }
                        None => {
                            run.hold_capture(file_name, body.clone(), ext);
                            let r = run.register_artifact(
                                kind,
                                None,
                                Some(size),
                                Some(sess.id.clone()),
                                format!(
                                    "screen capture {}x{} ({} format, held, ephemeral run)",
                                    screen.cols, screen.rows, ext
                                ),
                            )
                            .ok();
                            (None, r)
                        }
                    }
                };
                ok(json!({
                    "format": ext,
                    "dimensions": format!("{}x{}", screen.cols, screen.rows),
                    "saved_to": path,
                    "artifact": artifact.as_ref().map(|a| serde_json::json!({
                        "id": a.id,
                        "kind": a.kind,
                        "path": a.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                        "size": a.size,
                        "summary": a.summary,
                    })),
                    "held_in_run": path.is_none(),
                    "note": if is_svg { None } else {
                        Some("PNG glyphs cover printable ASCII; other scripts render as blocks — use format=svg for a faithful render".to_string())
                    },
                }))
            }
            }
        })
        .await
        .unwrap_or_else(|e| e)
}

/// Body of `tui_scenario` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_scenario(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiScenarioParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::ScenarioAction as SA;
    let Some(sc_action) = (match &p.action {
        crate::mcp::params::Known::Known(a) => Some(*a),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown scenario action '{}' (expected one of: {})",
                    o,
                    <SA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    match sc_action {
        SA::List => {
            let run = s.run.lock().unwrap();
            let recorded = run.active_recordings();
            let saved = run.list_saved_scenarios().unwrap_or_default();
            ok(json!({ "recordings_in_progress": recorded, "scenarios": saved }))
        }
        // Begin a recording bound to the target session's generation.
        // Subsequent tui_act / tui_wait / tui_assert calls resolving to
        // that session generation append steps; other sessions never do
        // (re-review item 5). The returned id — not the name — is the
        // identity for record_stop.
        SA::RecordStart => {
            let name = p.name.clone().unwrap_or_else(|| "scenario".into());
            let selector = p.id.clone();
            let run = s.run.clone();
            let started = s
                .with_sess(selector.as_deref(), move |sess| {
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    let rec_id = run
                        .lock()
                        .unwrap()
                        .begin_scenario_recording(&name, &sid, gen);
                    ok(json!({
                        "recording_id": rec_id.as_str(),
                        "name": name,
                        "session": sid,
                        "generation": gen,
                        "started": true,
                    }))
                })
                .await;
            started.unwrap_or_else(|e| e)
        }
        // Finish + persist. Accepts `recording_id` (preferred identity)
        // or falls back to the oldest active recording with `name`.
        SA::RecordStop => {
            let rec_id = match (&p.recording_id, &p.name) {
                (Some(id), _) => id.clone(),
                (None, Some(name)) => {
                    // Item 45: an ambiguous name is refused with the
                    // conflicting ids — never an oldest-match guess.
                    match s.run.lock().unwrap().find_recording_by_name(name) {
                        Ok(id) => id,
                        Err(ids) if ids.is_empty() => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!("no recording in progress named '{name}'"),
                            )
                        }
                        Err(ids) => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!(
                                    "'{name}' matches {} recordings in progress — pass \
                                         recording_id explicitly: {}",
                                    ids.len(),
                                    ids.join(", ")
                                ),
                            )
                        }
                    }
                }
                (None, None) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "record_stop requires 'recording_id' (or the recording 'name')",
                    )
                }
            };
            let mut run = s.run.lock().unwrap();
            if run.scenario_recording_step_count(&rec_id) == Some(0) {
                return err(
                    ErrorCategory::InvalidRequest,
                    "the recording has no steps; drive at least one act/intent/wait/assert before record_stop",
                );
            }
            match run.finish_scenario_recording(&rec_id) {
                Some(scenario) => {
                    let path: Option<String> = run
                        .save_scenario(scenario.clone())
                        .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
                    ok(json!({
                        "recording_id": rec_id,
                        "scenario_id": scenario.id,
                        "name": scenario.name,
                        "steps": scenario.step_count(),
                        "saved_to": path,
                        "scenario": serde_json::to_value(&scenario).unwrap_or_default(),
                    }))
                }
                None => err(
                    ErrorCategory::InvalidRequest,
                    format!("no recording in progress with id '{}'", rec_id),
                ),
            }
        }
        // explicit save of hand-authored steps (validated through the
        // Scenario model rather than stored as opaque JSON)
        SA::Save => {
            let name = p.name.clone().unwrap_or_else(|| "scenario".into());
            let steps = p.steps.clone().unwrap_or_default();
            if steps.is_empty() {
                return err(
                    ErrorCategory::InvalidRequest,
                    "a scenario must contain at least one step; use record_start/record_stop to capture a live flow or provide act/intent/wait/assert steps",
                );
            }
            let mut recorder = crate::scenario::recorder::ScenarioRecorder::new(name.clone());
            for s in &steps {
                // Canonical step shape = flat ({kind, ...params}), the
                // same shape record_stop emits and replay parses. A
                // nested {kind, params} object is also accepted so
                // hand-authored round-trips of an exported scenario's
                // internal form keep working.
                let kind = s
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("act")
                    .to_string();
                let params = match s.get("params") {
                    Some(nested) if nested.is_object() => {
                        let mut flat = s.clone();
                        if let Some(obj) = flat.as_object_mut() {
                            obj.remove("kind");
                            obj.remove("params");
                        }
                        let mut merged = nested.as_object().cloned().unwrap_or_default();
                        if let Some(flat_obj) = flat.as_object() {
                            for (k, v) in flat_obj {
                                merged.entry(k.clone()).or_insert(v.clone());
                            }
                        }
                        serde_json::Value::Object(merged)
                    }
                    _ => {
                        // Flat: everything except `kind`.
                        let mut flat = s.clone();
                        if let Some(obj) = flat.as_object_mut() {
                            obj.remove("kind");
                        }
                        flat
                    }
                };
                match kind.as_str() {
                    "act" => recorder.record_act(params),
                    "intent" => recorder.record_intent(params),
                    "wait" => recorder.record_wait(params),
                    "assert" => recorder.record_assert(params),
                    other => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("unknown step kind '{}' (act|intent|wait|assert)", other),
                        )
                    }
                }
            }
            let scenario = recorder.build();
            let count = scenario.step_count();
            let scenario_id = scenario.id.clone();
            let path: Option<String> = s
                .run
                .lock()
                .unwrap()
                .save_scenario(scenario.clone())
                .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
            ok(
                json!({ "scenario_id": scenario_id, "name": name, "steps": count, "saved_to": path }),
            )
        }
        SA::Export => {
            let name = match &p.name {
                Some(n) => n.clone(),
                None => return err(ErrorCategory::InvalidRequest, "export requires name"),
            };
            let run = s.run.lock().unwrap();
            match run.load_scenario(&name) {
                Ok(scenario) => ok(
                    json!({ "name": name, "scenario": serde_json::to_value(&scenario).unwrap_or_default() }),
                ),
                Err(e) => err(
                    ErrorCategory::InvalidRequest,
                    format!("no such scenario: {}", e),
                ),
            }
        }
        // Replay a saved scenario against a session through the one
        // canonical executor — the regression path: record once, run
        // again later, get a real pass/fail per step.
        SA::Run => {
            let name = match &p.name {
                Some(n) => n.clone(),
                None => return err(ErrorCategory::InvalidRequest, "run requires 'name'"),
            };
            let scenario = {
                let run = s.run.lock().unwrap();
                match run.load_scenario(&name) {
                    Ok(sc) => sc,
                    Err(e) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("no such scenario: {}", e),
                        )
                    }
                }
            };
            let selector = p.id.clone();
            let run = s.run.clone();
            // Audit P0-18: caller-supplied sensitive-parameter values, in
            // memory only — they ride into the runner and are never
            // recorded in the ledger or any artifact (the runner resolves
            // ${NAME} references before execution; only resolved steps
            // execute, and the ledger stores actions, not the replay's
            // input values).
            let parameter_values: Vec<crate::scenario::model::ParameterValue> = p
                .parameters
                .as_ref()
                .map(|m| {
                    m.iter()
                        .map(|(name, value)| crate::scenario::model::ParameterValue {
                            name: name.clone(),
                            value: value.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Finding 5: wire-level failure-policy override (stop|continue);
            // an unrecognized value is refused before anything runs.
            let policy_override: Option<crate::scenario::model::FailurePolicy> = match &p.on_failure
            {
                None => None,
                Some(known) => match known.known() {
                    Some(ScenarioFailurePolicy::Stop) => {
                        Some(crate::scenario::model::FailurePolicy::Stop)
                    }
                    Some(ScenarioFailurePolicy::Continue) => {
                        Some(crate::scenario::model::FailurePolicy::Continue)
                    }
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                            "unknown on_failure '{}' (expected one of: {})",
                            match known {
                                crate::mcp::params::Known::Other(o) => o.clone(),
                                _ => String::new(),
                            },
                            <ScenarioFailurePolicy as crate::mcp::params::EnumVariants>::VARIANTS
                                .join(", ")
                        ),
                        )
                    }
                },
            };
            s.with_sess(selector.as_deref(), move |sess| {
                // Wave G item 76: replay drives the app; a live human
                // lease refuses the whole run before step one.
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
                // Audit P0-21: the replay runs inside the run context, so
                // every executed act step lands in the transaction ledger
                // and frame evidence through the shared driving pipeline.
                let report = crate::scenario::runner::ScenarioRunner::run_in_run_with_policy(
                    &scenario,
                    sess,
                    &parameter_values,
                    Some(&run),
                    policy_override,
                );
                let (sid, gen) = (sess.id.clone(), sess.generation);
                let _ = run
                    .lock()
                    .unwrap()
                    .record_event(&sid, &format!("scenario_run:{}", scenario.name));
                let base = json!({
                    "name": report.scenario_name,
                    "session": sid,
                    "generation": gen,
                    "status": report.status,
                    "steps_total": report.steps_total,
                    "steps_passed": report.steps_passed,
                    "steps_failed": report.steps_failed,
                    "steps_skipped": report.steps_skipped,
                    "step_results": report.step_results,
                });
                if report.steps_failed == 0 && report.steps_skipped == 0 {
                    let mut v = base;
                    v["passed"] = json!(true);
                    ok(v)
                } else {
                    // Real regression (or a stop-policy halt): envelope stays
                    // success (transport ok), payload reports the failure
                    // honestly. `passed` is false the moment anything failed
                    // OR was skipped — a skipped step is not a pass.
                    let mut v = base;
                    v["passed"] = json!(false);
                    ok(v)
                }
            })
            .await
            .unwrap_or_else(|e| e)
        }
        // finding 39: synthesize regression assets from a finding's OWN
        // evidence + reproduction. Pure read — never drives, never records
        // new evidence. Every asset is stamped generated/inferred/
        // requires_review; a kind is only produced when the finding's
        // evidence can justify it; a finding with nothing to synthesize is
        // reported as such, never as an empty shell.
        SA::RegressionAsset => {
            let finding_id =
                match &p.finding_id {
                    Some(id) => id.clone(),
                    None => return err(
                        ErrorCategory::InvalidRequest,
                        "regression_asset requires 'finding_id' (from tui_audit or tui://findings)",
                    ),
                };
            let only = match &p.asset_type {
                None => None,
                Some(known) => match known.known() {
                    Some(t) => Some(match t {
                        crate::mcp::params::RegressionAssetType::Scenario => {
                            crate::scenario::regression_asset::RegressionAssetKind::Scenario
                        }
                        crate::mcp::params::RegressionAssetType::Assertion => {
                            crate::scenario::regression_asset::RegressionAssetKind::Assertion
                        }
                        crate::mcp::params::RegressionAssetType::ContractRule => {
                            crate::scenario::regression_asset::RegressionAssetKind::ContractRule
                        }
                        crate::mcp::params::RegressionAssetType::ViewportCase => {
                            crate::scenario::regression_asset::RegressionAssetKind::ViewportCase
                        }
                    }),
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "unknown asset_type '{}' (expected one of: {})",
                                match known {
                                    crate::mcp::params::Known::Other(o) => o.clone(),
                                    _ => String::new(),
                                },
                                <crate::mcp::params::RegressionAssetType as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                            ),
                        )
                    }
                },
            };

            // Pure reads: find the finding, gather the ledger + reproduction
            // loader (immutable borrow of the run), and generate the assets.
            // The immutable borrow is scoped so persistence can take the
            // mutable guard afterward.
            let (finding, assets) = {
                let run = s.run.lock().unwrap();
                let f = match run.findings().iter().find(|f| f.id == finding_id) {
                    Some(f) => f.clone(),
                    None => {
                        let labels = run.finding_baseline_labels();
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "unknown finding id '{finding_id}' in run '{}'. Record audits with label= to build baselines (stored: {})",
                                run.id(),
                                if labels.is_empty() { "none".to_string() } else { labels.join(", ") }
                            ),
                        );
                    }
                };
                let transactions = run.transactions().to_vec();
                let assets = {
                    let load = |key: &str| run.load_scenario(key).ok();
                    let ctx = crate::scenario::regression_asset::AssetContext {
                        load_scenario: &load,
                        transactions: &transactions,
                    };
                    crate::scenario::regression_asset::generate(&f, &ctx, only)
                };
                (f, assets)
            };
            // Persist the scenario-form assets (canonical, in-run) so the
            // reviewer can `tui_scenario action=export` them; always mark
            // the response with the review gate.
            let mut run = s.run.lock().unwrap();
            let mut persisted: Vec<serde_json::Value> = Vec::new();
            for a in &assets {
                let mut v = serde_json::to_value(a).unwrap_or_default();
                if let Some(sc) = a.as_scenario() {
                    let saved = run.save_scenario(sc.clone());
                    v["saved_to"] = saved
                        .map(|p| serde_json::Value::String(p.to_string_lossy().to_string()))
                        .unwrap_or(serde_json::Value::Null);
                    v["scenario_id"] = serde_json::Value::String(sc.id.clone());
                }
                persisted.push(v);
            }
            drop(run);
            if persisted.is_empty() {
                return ok(json!({
                    "workflow": "regression_asset",
                    "finding_id": finding_id,
                    "assets": [],
                    "generated": [],
                    "skipped": [
                        "scenario",
                        "assertion",
                        "contract_rule",
                        "viewport_case",
                    ],
                    "note": "finding '{finding_id}' has no reproduction, no transaction-ledger acts toward a concrete target, and no cited viewport — nothing to synthesize from (generation never fabricates evidence)",
                }));
            }
            ok(json!({
                "workflow": "regression_asset",
                "finding_id": finding_id,
                "rule": finding.rule_id.clone().unwrap_or_else(|| finding.id.clone()),
                "generated": persisted.iter().map(|a| a["kind"].clone()).collect::<Vec<_>>(),
                "assets": persisted,
                "review_gate": "all assets are generated/inferred and REQUIRE REVIEW before use — generation never auto-runs or promotes to required",
            }))
        }
    }
}
