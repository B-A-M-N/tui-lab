//! MCP surface: the 12 tools Hermes sees (spec section 13). Internally we have
//! hundreds of operations; the agent only sees `tui_session`, `tui_observe`,
//! `tui_act`, `tui_wait`, `tui_assert`, `tui_checkpoint`, `tui_scenario`,
//! `tui_record`, `tui_explore`, `tui_audit`, `tui_coverage`, `tui_framework`.

use std::sync::Arc;

use rmcp::serde_json::json;
use rmcp::tool;
use rmcp::tool_router;
use rmcp::ServerHandler;

use crate::error::ErrorCategory;
use crate::mcp::helpers::{build_input_from_request, build_wait, err, err_continued, ok, run_assertion};
use crate::mcp::params::*;
use crate::screen::diff;
use crate::semantic;
use crate::session::SessionManager;
use rmcp::handler::server::wrapper::Parameters;

/// Shared MCP state: the session manager plus the run context (composition
/// root for checkpoints, scenarios, recordings, findings, exploration graph —
/// audit item: "Run/Artifact Model"). Serialized by Hermes (no parallel calls).
#[derive(Clone)]
pub struct TuiLabServer {
    manager: Arc<std::sync::Mutex<SessionManager>>,
    run: Arc<std::sync::Mutex<crate::run::RunContext>>,
}

impl TuiLabServer {
    pub fn new() -> Self {
        TuiLabServer {
            manager: Arc::new(std::sync::Mutex::new(SessionManager::new())),
            run: Arc::new(std::sync::Mutex::new(crate::run::RunContext::ephemeral())),
        }
    }
}

impl Default for TuiLabServer {
    fn default() -> Self {
        TuiLabServer::new()
    }
}

#[tool_router(router = tool_router)]
impl TuiLabServer {
    /// Session lifecycle: start / restart / stop / list / status.
    #[tool(
        name = "tui_session",
        description = "Manage TUI sessions: start, restart, stop, list, status."
    )]
    async fn tui_session(&self, p: Parameters<TuiSessionParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        match p.action.as_str() {
            "start" => {
                let cols = p.cols.unwrap_or(80);
                let rows = p.rows.unwrap_or(24);
                let command = match &p.command {
                    Some(c) => c.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "start requires 'command'"),
                };
                // Honest backend/isolation negotiation (spec section 34). Do not
                // silently start portable-pty when an unsupported backend is asked.
                let backend = p.backend.clone().unwrap_or_else(|| "auto".into());
                let isolation = p.isolation.clone().unwrap_or_else(|| "local".into());
                match backend.as_str() {
                    "auto" | "portable_vt100" => {}
                    "tui_test" => {
                        return err(
                            ErrorCategory::Unsupported,
                            "backend 'tui_test' is not wired in this build; only 'auto'/'portable_vt100' are supported",
                        )
                    }
                    other => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("unknown backend '{}' (supported: auto, portable_vt100)", other),
                        )
                    }
                }
                match isolation.as_str() {
                    "local" => {}
                    "docker" => return err(
                        ErrorCategory::Unsupported,
                        "isolation 'docker' is not wired in this build; only 'local' is supported",
                    ),
                    other => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("unknown isolation '{}' (supported: local)", other),
                        )
                    }
                }
                let env: Vec<(String, String)> = p
                    .env
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                match mgr.start(
                    &command,
                    &p.args.unwrap_or_default(),
                    p.cwd.as_deref(),
                    &env,
                    cols,
                    rows,
                    &backend,
                    &isolation,
                ) {
                    Ok(id) => {
                        let sess = mgr.get_mut(&id).expect("just started");
                        let caps = sess.capabilities();
                        let launch = sess.launch().cloned();
                        let version = sess.backend_version();
                        let kind = sess.backend_kind.clone();
                        let generation = sess.generation;
                        ok(json!({
                            "session": id,
                            "generation": generation,
                            "backend": { "name": kind, "version": version },
                            "capabilities": caps,
                            "launch": launch,
                        }))
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            "restart" => {
                let id = match p.id.clone().or_else(|| mgr.active_id().map(str::to_string)) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session to restart"),
                };
                // Restart the SAME logical session: same id, next generation,
                // reusing the stored LaunchSpec (spec section 13).
                match mgr.restart(&id) {
                    Ok((new_id, generation)) => {
                        let sess = mgr.get_mut(&new_id).expect("just restarted");
                        let caps = sess.capabilities();
                        ok(json!({
                            "session": new_id,
                            "generation": generation,
                            "restarted_from": id,
                            "capabilities": caps,
                        }))
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            "stop" => {
                let id = match p.id.clone().or_else(|| mgr.active_id().map(str::to_string)) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                match mgr.stop(&id) {
                    Ok(()) => ok(json!({ "stopped": id })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            "list" => ok(json!({ "sessions": mgr.list() })),
            "status" => {
                let id = match p.id.clone().or_else(|| mgr.active_id().map(str::to_string)) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                match mgr.resolve_mut(Some(&id)) {
                    Ok(s) => {
                        let caps = s.capabilities();
                        ok(json!({
                            "session": id,
                            "command": s.command,
                            "backend": s.backend_kind,
                            "capabilities": caps,
                            "process": s.process(),
                        }))
                    }
                    Err(e) => err(ErrorCategory::NoSession, e.to_string()),
                }
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown action '{}'", other),
            ),
        }
    }

    /// Observe the screen: summary / screen / cells / region / semantic / diff / scrollback / history.
    #[tool(
        name = "tui_observe",
        description = "Observe terminal state. Modes: summary, screen, cells, semantic, diff, scrollback."
    )]
    async fn tui_observe(&self, p: Parameters<TuiObserveParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let screen = match sess.observe(p.idle_ms.unwrap_or(80)) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        match p.mode.as_deref().unwrap_or("summary") {
            "summary" => {
                // Compact summary for agent consumption (spec 36). Avoids
                // returning the full viewport text (which could be 160x50).
                // Use mode=screen for full text.
                let sem = semantic::analyze(&screen);
                let dialog_count = sem
                    .regions
                    .iter()
                    .filter(|r| r.kind == semantic::RegionKind::Dialog)
                    .count();
                let focused = sem.focus.control.clone();
                ok(json!({
                    "screen": format!("{}x{}", screen.cols, screen.rows),
                    "title": screen.title,
                    "process": {
                        "running": screen.process.running,
                        "exit_code": screen.process.exit_code,
                    },
                    "focus": focused,
                    "focus_confidence": sem.focus.confidence,
                    "regions": sem.regions.len(),
                    "controls": sem.controls.len(),
                    "dialogs": dialog_count,
                    "structure_hash": screen.structure_hash,
                    "visual_hash": screen.visual_hash,
                    "cursor": screen.cursor,
                }))
            }
            "screen" => ok(json!({ "viewport_text": screen.viewport_text })),
            "cells" => ok(json!({ "cells": screen.cells })),
            "semantic" => {
                let sem = semantic::analyze(&screen);
                ok(json!({ "semantic": sem }))
            }
            "diff" => {
                // Real screen diff against the last observed frame (spec 35/21).
                let before = sess.last().cloned();
                let tr = match before {
                    Some(ref b) => diff(b, &screen),
                    None => diff(&screen, &screen),
                };
                let semantic_diff = match &before {
                    Some(b) => {
                        let sem_before = semantic::analyze(b);
                        let sem_after = semantic::analyze(&screen);
                        json!({
                            "controls_added": sem_after.controls.len().saturating_sub(sem_before.controls.len()),
                            "controls_removed": sem_before.controls.len().saturating_sub(sem_after.controls.len()),
                            "regions_added": sem_after.regions.len().saturating_sub(sem_before.regions.len()),
                            "regions_removed": sem_before.regions.len().saturating_sub(sem_after.regions.len()),
                            "focus_before": sem_before.focus.control,
                            "focus_after": sem_after.focus.control,
                        })
                    }
                    None => json!({ "note": "no prior frame available" }),
                };
                ok(json!({
                    "since": "last_observation",
                    "structure_hash": screen.structure_hash,
                    "visual_hash": screen.visual_hash,
                    "raw_hash": screen.raw_hash,
                    "screen_diff": tr,
                    "semantic_diff": semantic_diff,
                }))
            }
            "scrollback" => ok(json!({ "scrollback": screen.scrollback })),
            "history" => ok(json!({ "structure_hash": screen.structure_hash })),
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown mode '{}'", other),
            ),
        }
    }

    /// Drive input: key / keys / type / paste / mouse_* / resize / signal / raw.
    /// After acting, returns a BEFORE/AFTER transition (spec section 11/30).
    #[tool(
        name = "tui_act",
        description = "Drive input. Returns a screen transition after the action. Actions: key, keys, type, paste, raw, mouse_click, mouse_press, mouse_release, mouse_move, mouse_drag, mouse_scroll, resize, signal."
    )]
    async fn tui_act(&self, p: Parameters<TuiActRequest>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let before = match sess.last().cloned() {
            Some(s) => s,
            None => match sess.observe(0) {
                Ok(s) => s,
                Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
            },
        };
        let input = match build_input_from_request(&p) {
            Ok(i) => i,
            Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
        };
        if let Err(e) = sess.send(input) {
            return err(ErrorCategory::BackendError, e.to_string());
        }
        // default: wait for idle + capture transition
        if !p.no_wait() {
            let _ = sess.wait(
                crate::backend::WaitCond::ScreenStable {
                    quiet_for: std::time::Duration::from_millis(p.wait_ms().unwrap_or(150)),
                    after_screen_seq: None,
                },
                p.wait_ms().unwrap_or(150) + 1000,
            );
        }
        let after = match sess.observe(p.wait_ms().unwrap_or(150)) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        let tr = diff(&before, &after);
        // Scenario recording in progress? Append this act (audit: scenarios
        // capture real tool traffic; sensitive payloads are not recorded).
        if !p.sensitive() {
            let mut run = self.run.lock().unwrap();
            for name in run.active_recordings() {
                run.record_scenario_act(
                    &name,
                    serde_json::to_value(&p).unwrap_or_default(),
                );
            }
        }
        ok(json!({
            "action": p.action_name(),
            "transition": tr,
        }))
    }

    /// Wait for a state condition without fixed sleeps.
    #[tool(
        name = "tui_wait",
        description = "Block until a condition holds: text, text_absent, screen_change, screen_stable, process_exit, title, bell."
    )]
    async fn tui_wait(&self, p: Parameters<TuiWaitParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let cond = match build_wait(&p) {
            Some(c) => c,
            None => return err(ErrorCategory::InvalidRequest, "unsupported wait condition"),
        };
        match sess.wait(cond, p.budget_ms.unwrap_or(5000)) {
            Ok(out) => {
                {
                    let mut run = self.run.lock().unwrap();
                    for name in run.active_recordings() {
                        run.record_scenario_wait(
                            &name,
                            serde_json::to_value(&p).unwrap_or_default(),
                        );
                    }
                }
                ok(json!({
                    "met": out.met,
                    "timeout": !out.met,
                    "reason": format!("{:?}", out.reason),
                    "elapsed_ms": out.elapsed_ms,
                    "screen_seq": out.screen_seq,
                    "output_seq": out.output_seq,
                    "state": {
                        "structure_hash": out.state.structure_hash,
                        "visual_hash": out.state.visual_hash,
                        "process": {
                            "running": out.state.process.running,
                            "exit_code": out.state.process.exit_code,
                        },
                    },
                }))
            }
            Err(e) => err(ErrorCategory::BackendError, e.to_string()),
        }
    }

    /// Assertions against screen/state.
    #[tool(
        name = "tui_assert",
        description = "Assert UI facts: text, text_absent, position, focus, not_clipped, dimensions, exit_code."
    )]
    async fn tui_assert(&self, p: Parameters<TuiAssertParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let screen = match sess.observe(40) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        let (passed, detail, invalid) = run_assertion(&p, &screen);
        {
            let mut run = self.run.lock().unwrap();
            for name in run.active_recordings() {
                run.record_scenario_assert(&name, serde_json::to_value(&p).unwrap_or_default());
            }
        }
        if passed {
            ok(json!({ "passed": true, "assertion": p.assertion }))
        } else if let Some(ErrorCategory::InvalidRequest) = invalid {
            // Unknown assertion name (or missing required param): a caller error,
            // NOT a UI failure (spec section 37).
            err(ErrorCategory::InvalidRequest, detail)
        } else {
            err_continued(ErrorCategory::AssertionFailed, detail)
        }
    }

    /// Checkpoints: save / compare / list / delete (spec section 13).
    #[tool(
        name = "tui_checkpoint",
        description = "Save and compare named UI state checkpoints."
    )]
    async fn tui_checkpoint(&self, p: Parameters<TuiCheckpointParams>) -> String {
        let p = p.0;
        // Resolve session first (all actions except a bare list still need a
        // live session; list is per-session scoped, defaulting to active).
        let mut mgr = self.manager.lock().unwrap();
        let session_id = match mgr.resolve(p.id.as_deref()) {
            Ok(s) => s.id.clone(),
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let mut run = self.run.lock().unwrap();
        match p.action.as_str() {
            "list" => ok(json!({ "checkpoints": run.checkpoints.list(&session_id) })),
            "save" => {
                let (generation, screen, sem) = {
                    let sess = match mgr.get_mut(&session_id) {
                        Ok(s) => s,
                        Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                    };
                    let screen = match sess.observe(40) {
                        Ok(s) => s,
                        Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                    };
                    let sem = semantic::analyze(&screen);
                    (sess.generation, screen, sem)
                };
                let name = run.checkpoints.save(
                    &session_id,
                    generation,
                    p.name.clone(),
                    &screen,
                    Some(&sem),
                );
                ok(json!({
                    "name": name,
                    "structure_hash": screen.structure_hash,
                    "visual_hash": screen.visual_hash,
                    "focus": sem.focus.control,
                    "controls": sem.controls.len(),
                }))
            }
            "compare" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "compare requires 'name'"),
                };
                let sess = match mgr.get_mut(&session_id) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                };
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let sem = semantic::analyze(&screen);
                match run.checkpoints.compare(&session_id, &name, &screen, Some(&sem)) {
                    Ok(json_out) => json_out,
                    Err(ErrorCategory::InvalidRequest) => {
                        err(ErrorCategory::InvalidRequest, "no such checkpoint")
                    }
                    Err(c) => err(c, "checkpoint comparison failed"),
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "delete requires 'name'"),
                };
                let removed = run.checkpoints.delete(&session_id, &name);
                ok(json!({ "name": name, "deleted": removed }))
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown action '{}'", other),
            ),
        }
    }

    /// Scenarios: record discovered workflows (spec section 13 / 4.4).
    #[tool(
        name = "tui_scenario",
        description = "Record, save, list, and export workflows as regression scenarios."
    )]
    async fn tui_scenario(&self, p: Parameters<TuiScenarioParams>) -> String {
        let p = p.0;
        let mut run = self.run.lock().unwrap();
        match p.action.as_str() {
            "list" => {
                let recorded = run.active_recordings();
                let saved = run.list_saved_scenarios().unwrap_or_default();
                ok(json!({ "recordings_in_progress": recorded, "scenarios": saved }))
            }
            // begin a recording: subsequent tui_act / tui_wait / tui_assert
            // calls with the same recording name append steps (audit item:
            // scenarios must record real tool traffic, not be hand-authored).
            "record_start" => {
                let name = p.name.clone().unwrap_or_else(|| "scenario".into());
                if run.begin_scenario_recording(&name) {
                    ok(json!({ "recording": name, "started": true }))
                } else {
                    err(
                        ErrorCategory::InvalidRequest,
                        format!("already recording scenario '{}'", name),
                    )
                }
            }
            // finish + persist; returns the serialized Scenario
            "record_stop" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => {
                        return err(ErrorCategory::InvalidRequest, "record_stop requires 'name'")
                    }
                };
                match run.finish_scenario_recording(&name) {
                    Some(scenario) => {
                        let path: Option<String> = run
                            .save_scenario(scenario.clone())
                            .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
                        ok(json!({
                            "name": scenario.name,
                            "steps": scenario.step_count(),
                            "saved_to": path,
                            "scenario": serde_json::to_value(&scenario).unwrap_or_default(),
                        }))
                    }
                    None => err(
                        ErrorCategory::InvalidRequest,
                        format!("no recording in progress named '{}'", name),
                    ),
                }
            }
            // explicit save of hand-authored steps (validated through the
            // Scenario model rather than stored as opaque JSON)
            "save" => {
                let name = p.name.clone().unwrap_or_else(|| "scenario".into());
                let steps = p.steps.clone().unwrap_or_default();
                let mut recorder = crate::scenario::recorder::ScenarioRecorder::new(name.clone());
                for s in &steps {
                    let kind = s
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or("act")
                        .to_string();
                    let params = s.get("params").cloned().unwrap_or(serde_json::Value::Null);
                    match kind.as_str() {
                        "act" => recorder.record_act(params),
                        "wait" => recorder.record_wait(params),
                        "assert" => recorder.record_assert(params),
                        other => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!("unknown step kind '{}' (act|wait|assert)", other),
                            )
                        }
                    }
                }
                let scenario = recorder.build();
                let count = scenario.step_count();
                let path: Option<String> = run
                    .save_scenario(scenario.clone())
                    .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
                ok(json!({ "name": name, "steps": count, "saved_to": path }))
            }
            "export" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "export requires name"),
                };
                match run.load_scenario(&name) {
                    Ok(scenario) => ok(
                        json!({ "name": name, "scenario": serde_json::to_value(&scenario).unwrap_or_default() }),
                    ),
                    Err(e) => err(ErrorCategory::InvalidRequest, format!("no such scenario: {}", e)),
                }
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown action '{}'", other),
            ),
        }
    }

    /// Recordings: cast / svg / apng / gif / mp4 (spec section 13). Microsoft
    /// provides raster capture; here we expose the asciinema `.cast` writer.
    #[tool(
        name = "tui_record",
        description = "Produce terminal recordings (asciinema .cast). Other formats are delegated to the backend."
    )]
    async fn tui_record(&self, p: Parameters<TuiRecordParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        match p.format.as_deref().unwrap_or("start") {
            // Attach the raw PTY hook (audit item 24): every byte the reader
            // thread sees from now on is captured with timing.
            "start" => {
                sess.enable_recording(false);
                ok(json!({
                    "recording": "started",
                    "boundary": "pty-bytes",
                    "note": "output is captured at the raw PTY byte boundary; call format=stop to flush to a .cast file"
                }))
            }
            // Detach + write the .cast into the run's recordings dir.
            "stop" => {
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
                let run = self.run.lock().unwrap();
                let path = match run.run_dir() {
                    Some(dir) => {
                        let rec_dir = dir.join("recordings");
                        let _ = std::fs::create_dir_all(&rec_dir);
                        let file = rec_dir.join(format!(
                            "{}-{}.cast",
                            sess.id,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0)
                        ));
                        match std::fs::write(&file, ndjson.join("\n") + "\n") {
                            Ok(()) => Some(file.to_string_lossy().to_string()),
                            Err(e) => {
                                return err(
                                    ErrorCategory::BackendError,
                                    format!("recording flush failed: {}", e),
                                )
                            }
                        }
                    }
                    None => None,
                };
                ok(json!({
                    "recording": "stopped",
                    "events": events,
                    "saved_to": path,
                    "note": path.is_none().then(|| "ephemeral run: content returned inline, not persisted".to_string()),
                    "inline_events": path.is_none().then_some(ndjson),
                }))
            }
            "cast" => err(
                ErrorCategory::InvalidRequest,
                "format='cast' is not a lifecycle action; use format=start then format=stop (produces asciinema v3 .cast)",
            ),
            other => err(
                ErrorCategory::Unsupported,
                format!(
                    "recording format '{}' is not implemented by the current backend; only the pty-boundary .cast lifecycle (start/stop) is available",
                    other
                ),
            ),
        }
    }

    /// Exploration: random / guided_candidates / coverage_guided / replay (spec section 4/25).
    #[tool(
        name = "tui_explore",
        description = "Seeded random exploration, candidate generation, or replay. Returns evidence, not another reasoning loop."
    )]
    async fn tui_explore(&self, p: Parameters<TuiExploreParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        match p.mode.as_str() {
            "guided_candidates" => {
                // return novel action candidates for Hermes to choose (spec 4.3)
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let sem = semantic::analyze(&screen);
                let candidates = crate::exploration::candidates::suggest(&screen, &sem);
                ok(json!({ "novel_actions": candidates }))
            }
            "random" => {
                let seed = p.seed.unwrap_or(4242);
                let actions = p.actions.unwrap_or(20);
                let recording_path = p.recording_path.as_deref().map(std::path::Path::new);
                if recording_path.is_some() {
                    sess.enable_recording(false);
                }
                match crate::exploration::random::run(sess, seed, actions, recording_path) {
                    Ok(report) => {
                        // Feed the observed structure hashes into the run's
                        // state graph (consecutive hashes become transitions;
                        // the pool does not name targets, so transitions carry
                        // the action sequence index).
                        let graph_summary = {
                            let mut run = self.run.lock().unwrap();
                            let mut prev: Option<&str> = None;
                            for (i, h) in report.structure_hashes.iter().enumerate() {
                                run.state_graph.record_state(h, None, i as u64);
                                if let Some(p) = prev {
                                    if p != h {
                                        run.state_graph
                                            .record_transition(p, h, &format!("step-{}", i));
                                    }
                                }
                                prev = Some(h.as_str());
                            }
                            json!({
                                "states": run.state_graph.state_count(),
                                "transitions": run.state_graph.transition_count(),
                                "dead_ends": run.state_graph.find_dead_ends().len(),
                            })
                        };
                        // Persist the graph when the run is persistent.
                        let graph_path = {
                            let run = self.run.lock().unwrap();
                            match run.run_dir() {
                                Some(dir) => {
                                    let path = dir.join("state_graph.json");
                                    let payload = json!({
                                        "edges": run.state_graph.edge_list(),
                                        "visit_counts": run.state_graph.visit_counts(),
                                        "known_states": run.state_graph.known_states(),
                                    });
                                    std::fs::write(&path, payload.to_string())
                                        .ok()
                                        .map(|_| path.to_string_lossy().to_string())
                                }
                                None => None,
                            }
                        };
                        ok(json!({
                            "seed": seed,
                            "report": report,
                            "state_graph": graph_summary,
                            "state_graph_path": graph_path,
                        }))
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            "state_graph" => {
                let run = self.run.lock().unwrap();
                ok(json!({
                    "states": run.state_graph.state_count(),
                    "transitions": run.state_graph.transition_count(),
                    "dead_ends": run.state_graph
                        .find_dead_ends()
                        .into_iter()
                        .map(|n| n.structure_hash.clone())
                        .collect::<Vec<String>>(),
                    "edges": run.state_graph.edge_list(),
                    "visit_counts": run.state_graph.visit_counts(),
                    "budget_exhausted": run.state_graph.budget_exhausted(),
                }))
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown mode '{}'", other),
            ),
        }
    }

    /// UX audits: keyboard / focus / layout / resize / navigation / discoverability / states / errors / mouse / color / performance / full (spec section 14-21).
    #[tool(
        name = "tui_audit",
        description = "Run deterministic UX audits and return evidence-backed findings."
    )]
    async fn tui_audit(&self, p: Parameters<TuiAuditParams>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id.as_deref()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        let profile = p.profile.as_deref().unwrap_or("full").to_string();

        // Active profiles drive the app through the session and observe real
        // transitions (audit items 52-55). Static profiles read one frame.
        let active = matches!(
            profile.as_str(),
            "keyboard" | "focus" | "resize" | "clipping" | "layout"
        );
        let (findings, mode) = if active {
            let fs = match profile.as_str() {
                "keyboard" => crate::audit::driver::keyboard_audit(sess, 20),
                "focus" => crate::audit::driver::focus_audit(sess),
                "resize" | "layout" => crate::audit::driver::resize_audit(sess),
                "clipping" => crate::audit::driver::clipping_audit(sess),
                _ => unreachable!("guarded by `active`"),
            };
            (fs, "active")
        } else {
            let screen = match sess.observe(40) {
                Ok(s) => s,
                Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
            };
            let sem = semantic::analyze(&screen);
            (
                crate::audit::run(&profile, &screen, &sem),
                "static",
            )
        };

        // Findings accumulate in the run context (composition root) so a
        // later audit/coverage query can see prior evidence.
        {
            let mut run = self.run.lock().unwrap();
            run.extend_findings(findings.clone());
        }
        ok(json!({
            "profile": profile,
            "mode": mode,
            "finding_count": findings.len(),
            "findings": findings,
        }))
    }

    /// Coverage (spec section 5). Backed by `tuicov` when present; reports
    /// "unavailable" otherwise (it is not a published crate — §36/§51).
    #[tool(
        name = "tui_coverage",
        description = "Native coverage via optional tuicov executable. Reports availability honestly."
    )]
    async fn tui_coverage(&self, p: Parameters<TuiCoverageParams>) -> String {
        let p = p.0;
        match crate::coverage::tuicov::handle(&p) {
            Ok(s) => s,
            Err(e) => err(ErrorCategory::BackendError, e.to_string()),
        }
    }

    /// Framework detection + native adapters (spec section 26/27).
    #[tool(
        name = "tui_framework",
        description = "Detect the TUI framework and (optionally) run native probes."
    )]
    async fn tui_framework(&self, p: Parameters<TuiFrameworkParams>) -> String {
        let p = p.0;
        let cwd = p.cwd.clone().unwrap_or_else(|| ".".into());
        let det = crate::framework::detect::detect(&cwd);
        match p.action.as_str() {
            "detect" => ok(json!({ "framework": det })),
            "capabilities" => ok(
                json!({ "framework": det, "note": "native probes invoke project-local tooling" }),
            ),
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown action '{}'", other),
            ),
        }
    }
}

// Generate `call_tool`/`list_tools`/`get_info` from the tool router above.
#[rmcp::tool_handler]
impl ServerHandler for TuiLabServer {}
