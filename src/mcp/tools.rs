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
use crate::mcp::helpers::{
    build_input_from_request, build_wait, checkpoints, err, err_continued, ok, run_assertion,
    scenarios, write_cast,
};
use crate::mcp::params::*;
use crate::screen::diff;
use crate::semantic;
use crate::session::SessionManager;
use rmcp::handler::server::wrapper::Parameters;

/// Shared MCP state: the session manager. Serialized by Hermes (no parallel calls).
#[derive(Clone, Default)]
pub struct TuiLabServer {
    manager: Arc<std::sync::Mutex<SessionManager>>,
}

impl TuiLabServer {
    pub fn new() -> Self {
        TuiLabServer {
            manager: Arc::new(std::sync::Mutex::new(SessionManager::new())),
        }
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
                    .map(|(k, v)| (k, v))
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
                        let sess = mgr.get(&id).expect("just started");
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
                        let sess = mgr.get(&new_id).expect("just restarted");
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
                },
                p.wait_ms().unwrap_or(150) + 1000,
            );
        }
        let after = match sess.observe(p.wait_ms().unwrap_or(150)) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        let tr = diff(&before, &after);
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
            Ok(true) => ok(json!({ "met": true })),
            Ok(false) => ok(json!({ "met": false, "timeout": true })),
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
        match p.action.as_str() {
            "list" => ok(
                json!({ "checkpoints": checkpoints().lock().unwrap().keys().collect::<Vec<_>>() }),
            ),
            "save" => {
                let mut mgr = self.manager.lock().unwrap();
                let sess = match mgr.resolve_mut(p.id.as_deref()) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                };
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let name = p
                    .name
                    .clone()
                    .unwrap_or_else(|| screen.structure_hash.clone());
                checkpoints()
                    .lock()
                    .unwrap()
                    .insert(name.clone(), screen.structure_hash.clone());
                ok(json!({ "name": name, "structure_hash": screen.structure_hash }))
            }
            "compare" => {
                let mut mgr = self.manager.lock().unwrap();
                let sess = match mgr.resolve_mut(p.id.as_deref()) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                };
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                match p
                    .name
                    .as_ref()
                    .and_then(|n| checkpoints().lock().unwrap().get(n).cloned())
                {
                    Some(prev) => {
                        ok(json!({ "name": p.name, "match": prev == screen.structure_hash }))
                    }
                    None => err(ErrorCategory::InvalidRequest, "no such checkpoint"),
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "delete requires 'name'"),
                };
                let removed = checkpoints().lock().unwrap().remove(&name).is_some();
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
        match p.action.as_str() {
            "list" => {
                ok(json!({ "scenarios": scenarios().lock().unwrap().keys().collect::<Vec<_>>() }))
            }
            "save" => {
                let name = p.name.clone().unwrap_or_else(|| "scenario".into());
                let steps = p.steps.clone().unwrap_or_default();
                scenarios().lock().unwrap().insert(name.clone(), steps);
                ok(
                    json!({ "name": name, "steps": scenarios().lock().unwrap().get(&name).unwrap().len() }),
                )
            }
            "export" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "export requires name"),
                };
                match scenarios().lock().unwrap().get(&name) {
                    Some(s) => ok(json!({ "name": name, "scenario": s })),
                    None => err(ErrorCategory::InvalidRequest, "no such scenario"),
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
        match p.format.as_deref().unwrap_or("cast") {
            "cast" => {
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let out = write_cast(&screen);
                ok(
                    json!({ "format": "cast", "event_count": out.len(), "note": "asciinema v3 NDJSON events buffered" }),
                )
            }
            other => err(
                ErrorCategory::Unsupported,
                format!(
                    "recording format '{}' is not implemented by the current backend; only 'cast' (asciinema v3) is available",
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
                    Ok(report) => ok(json!({ "seed": seed, "report": report })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
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
        let screen = match sess.observe(40) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        let sem = semantic::analyze(&screen);
        let findings = crate::audit::run(p.profile.as_deref().unwrap_or("full"), &screen, &sem);
        ok(json!({ "profile": p.profile, "findings": findings }))
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
