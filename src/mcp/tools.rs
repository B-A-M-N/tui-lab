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
                        // Attach the launch spec to the run (audit re-review
                        // item 1: the run owns session/launch correlation, so
                        // relaunches and artifacts share one identity).
                        if let Some(spec) = launch.clone() {
                            self.run.lock().unwrap().set_launch_spec(spec);
                        }
                        let run_id = self.run.lock().unwrap().id.clone();
                        ok(json!({
                            "session": id,
                            "generation": generation,
                            "run": run_id,
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
        self.run.lock().unwrap().bump_event();

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
                // Run focus graph: ledger this observation's focus against
                // the previous observation's (per session).
                {
                    let prev_focus = sess
                        .previous()
                        .map(|p| semantic::analyze(p).focus.control);
                    self.run.lock().unwrap().record_focus_transition(
                        &sess.id,
                        prev_focus.unwrap_or(None),
                        focused.clone(),
                    );
                }
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
                // ONE diff path (re-review item 7): observe() above stashed
                // the prior frame in `previous`, so this is a real
                // previous→current comparison through the canonical
                // `screen::diff`, returning the same `Transition` shape used
                // by InteractionTransaction (semantic diff included — no
                // second, MCP-local semantic-diff implementation).
                match sess.previous() {
                    Some(before) => {
                        let tr = diff(before, &screen);
                        ok(json!({
                            "since": "previous_observation",
                            "transition": tr,
                        }))
                    }
                    None => ok(json!({
                        "since": null,
                        "note": "no prior frame available; observe twice, then diff",
                        "structure_hash": screen.structure_hash,
                        "visual_hash": screen.visual_hash,
                    })),
                }
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
        let input = match build_input_from_request(&p) {
            Ok(i) => i,
            Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
        };
        // The one canonical executor (re-review item 4): anchored settle wait
        // (item 8) + honest settle reporting (item 9).
        let quiet = p.wait_ms().unwrap_or(150);
        let tx = match crate::execution::execute_act(
            sess,
            p.action_name(),
            input,
            quiet,
            quiet.saturating_add(1000),
            p.no_wait(),
        ) {
            Ok(t) => t,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        // Scenario recording in progress? Append this act (audit: scenarios
        // capture real tool traffic; sensitive payloads are not recorded).
        // Scoped to the resolved session generation (re-review item 5): a
        // recording for session A never absorbs session B's traffic.
        {
            let mut run = self.run.lock().unwrap();
            run.bump_transaction();
            if !p.sensitive() {
                let (sid, gen) = (sess.id.clone(), sess.generation);
                run.record_scenario_act(&sid, gen, serde_json::to_value(&p).unwrap_or_default());
            }
        }
        ok(json!({
            "action": tx.action,
            "settled": tx.settled,
            "settle_reason": tx.settle_reason,
            "elapsed_ms": tx.elapsed_ms,
            "warnings": if tx.settled { Vec::<String>::new() } else {
                vec!["screen did not reach the requested stability within the settle budget".to_string()]
            },
            "transition": tx.transition,
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
                // Scoped to the resolved session generation (item 5).
                {
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    let mut run = self.run.lock().unwrap();
                    run.bump_transaction();
                    run.record_scenario_wait(
                        &sid,
                        gen,
                        serde_json::to_value(&p).unwrap_or_default(),
                    );
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
            // Scoped to the resolved session generation (item 5).
            let (sid, gen) = (sess.id.clone(), sess.generation);
            let mut run = self.run.lock().unwrap();
            run.record_scenario_assert(&sid, gen, serde_json::to_value(&p).unwrap_or_default());
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
            // Begin a recording bound to the target session's generation.
            // Subsequent tui_act / tui_wait / tui_assert calls resolving to
            // that session generation append steps; other sessions never do
            // (re-review item 5). The returned id — not the name — is the
            // identity for record_stop.
            "record_start" => {
                let mut mgr = self.manager.lock().unwrap();
                let sess = match mgr.resolve_mut(p.id.as_deref()) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                };
                let name = p.name.clone().unwrap_or_else(|| "scenario".into());
                let (sid, gen) = (sess.id.clone(), sess.generation);
                drop(mgr);
                let rec_id = run.begin_scenario_recording(&name, &sid, gen);
                ok(json!({
                    "recording_id": rec_id.as_str(),
                    "name": name,
                    "session": sid,
                    "generation": gen,
                    "started": true,
                }))
            }
            // Finish + persist. Accepts `recording_id` (preferred identity)
            // or falls back to the oldest active recording with `name`.
            "record_stop" => {
                let rec_id = match (&p.recording_id, &p.name) {
                    (Some(id), _) => id.clone(),
                    (None, Some(name)) => match run.find_recording_by_name(name) {
                        Some(id) => id,
                        None => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!("no recording in progress named '{}'", name),
                            )
                        }
                    },
                    (None, None) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            "record_stop requires 'recording_id' (or the recording 'name')",
                        )
                    }
                };
                match run.finish_scenario_recording(&rec_id) {
                    Some(scenario) => {
                        let path: Option<String> = run
                            .save_scenario(scenario.clone())
                            .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
                        ok(json!({
                            "recording_id": rec_id,
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
                let path = {
                    let mut run = self.run.lock().unwrap();
                    match run.run_dir().cloned() {
                        Some(dir) => {
                            let rec_dir = dir.join("recordings");
                            let _ = std::fs::create_dir_all(&rec_dir);
                            let file = rec_dir.join(&file_name);
                            match std::fs::write(&file, &body) {
                                Ok(()) => Some(file.to_string_lossy().to_string()),
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
                            None
                        }
                    }
                };
                ok(json!({
                    "recording": "stopped",
                    "events": events,
                    "saved_to": path,
                    "held_in_run": path.is_none(),
                    "note": path.is_none().then(|| "ephemeral run: held in run memory; tui_run persist will write it to the durable root".to_string()),
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
                let recording_path = p.recording_path.as_deref().map(std::path::Path::new);
                if recording_path.is_some() {
                    sess.enable_recording(false);
                }
                // The budget is the authority (re-review item 13): limits come
                // from the run's ExplorationBudget, with an optional action
                // override; the report names the real completion reason.
                let budget = {
                    let run = self.run.lock().unwrap();
                    crate::exploration::random::Budget {
                        max_actions: p.actions.unwrap_or(run.state_graph.budget().max_actions),
                        ..crate::exploration::random::Budget::from_graph_budget(
                            run.state_graph.budget(),
                        )
                    }
                };
                match crate::exploration::random::run(sess, seed, budget, recording_path) {
                    Ok(report) => {
                        // The state graph records WHAT ACTUALLY HAPPENED
                        // (re-review item 12): transitions come from the
                        // ordered ExplorationStep records (before → after via
                        // the real action), not from post-hoc hash lists.
                        let graph_summary = {
                            let mut run = self.run.lock().unwrap();
                            crate::exploration::random::record_steps(
                                &mut run.state_graph,
                                &report.steps,
                            );
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

    /// Explicit run lifecycle (goal spec). Server startup stays side-effect
    /// free: `TuiLabServer::new()` opens an ephemeral run — no filesystem
    /// mutation. `persist` promotes the SAME run (identity + everything
    /// accumulated so far) to durable storage; the root resolves from the
    /// primary session's `LaunchSpec.cwd` unless an explicit root is given —
    /// never from this process's cwd. `close` flushes and marks closed; it
    /// does not kill sessions unless `kill_sessions` is set.
    #[tool(
        name = "tui_run",
        description = "Run lifecycle: status, persist (ephemeral→durable, same run identity), close. Nothing is written to disk until you persist."
    )]
    async fn tui_run(&self, p: Parameters<TuiRunParams>) -> String {
        let p = p.0;
        match p.action.as_str() {
            "status" => {
                let sessions = self.manager.lock().unwrap().list();
                ok(self.run.lock().unwrap().status(
                    sessions.into_iter().map(serde_json::Value::String).collect(),
                ))
            }
            "persist" => {
                let mut run = self.run.lock().unwrap();
                if run.run_dir().is_some() {
                    return ok(json!({
                        "run_id": run.id,
                        "already_persistent": true,
                        "artifact_root": run.run_dir().map(|d| d.to_string_lossy().to_string()),
                    }));
                }
                // Root resolution: explicit `root` wins; else the primary
                // session's LaunchSpec.cwd; else invalid_request — do NOT
                // guess from the server process cwd.
                let base: String = match p.root.clone() {
                    Some(r) => r,
                    None => match run.primary_session_cwd() {
                        Some(cwd) => cwd.to_string(),
                        None => {
                            let run_id = run.id.clone();
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!(
                                    "no artifact root resolvable for run {}: pass 'root' explicitly, or start a session whose LaunchSpec.cwd is set",
                                    run_id
                                ),
                            );
                        }
                    },
                };
                match run.promote(std::path::Path::new(&base)) {
                    Ok(root) => ok(json!({
                        "run_id": run.id,
                        "persistent": true,
                        "artifact_root": root.to_string_lossy(),
                        "promoted_from_ephemeral": true,
                    })),
                    Err(e) => err(ErrorCategory::InternalError, format!("persist failed: {e}")),
                }
            }
            "close" => {
                let mut run = self.run.lock().unwrap();
                let already = run.is_closed();
                let sessions = self.manager.lock().unwrap().list();
                let summary =
                    run.status(sessions.into_iter().map(serde_json::Value::String).collect());
                let kill = p.kill_sessions.unwrap_or(false);
                let result = run.close();
                drop(run);
                if let Err(e) = result {
                    return err(ErrorCategory::InternalError, format!("close flush failed: {e}"));
                }
                // Sessions survive close unless explicitly requested.
                let stopped: Vec<String> = if kill {
                    let mut mgr = self.manager.lock().unwrap();
                    let ids: Vec<String> = mgr.list();
                    let mut stopped = Vec::new();
                    for id in ids {
                        if mgr.stop(&id).is_ok() {
                            stopped.push(id);
                        }
                    }
                    stopped
                } else {
                    Vec::new()
                };
                ok(json!({
                    "closed": true,
                    "already_closed": already,
                    "sessions_stopped": stopped,
                    "final": summary,
                }))
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown run action '{}' (status|persist|close)", other),
            ),
        }
    }
}

// Generate `call_tool`/`list_tools`/`get_info` from the tool router above.
#[rmcp::tool_handler]
impl ServerHandler for TuiLabServer {}
