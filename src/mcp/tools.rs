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
use crate::mcp::helpers::{build_wait, err, err_continued, ok};
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
    pub async fn tui_session(&self, p: Parameters<TuiSessionParams>) -> String {
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
                let env: Vec<(String, String)> = p.env.unwrap_or_default().into_iter().collect();
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
                        // Attach the launch spec to the run, per session
                        // (audit re-review item 1 + P0 fix 6: the run owns
                        // session/launch correlation for *all* sessions, so
                        // relaunches, restarts, and multi-session replay
                        // share one identity).
                        if let Some(spec) = launch.clone() {
                            self.run.lock().unwrap().set_launch_spec(&id, spec);
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
        description = "Observe terminal state. Modes: summary, screen, cells, semantic, tree, nodes, diff, scrollback."
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
                // the previous observation's (per session). Both ledgers get
                // fed (Wave D item 36): the legacy label list and the
                // ID-keyed FocusGraph, the latter only when both frames
                // resolved stable control IDs.
                {
                    let prev = sess
                        .previous()
                        .map(|p| semantic::analyze(p).focus)
                        .unwrap_or_default();
                    let mut run = self.run.lock().unwrap();
                    run.record_focus_observation(
                        &sess.id,
                        prev.control.clone(),
                        focused.clone(),
                        prev.control_id.as_deref(),
                        sem.focus.control_id.as_deref(),
                        "unknown",
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
            // The hierarchical rendering (re-review Wave-4): regions nested
            // per containment, controls inside their regions, focus and
            // screen-level components attached. This is the shape to compare
            // against an intended design without re-deriving containment.
            "tree" => {
                let sem = semantic::analyze(&screen);
                let tree = semantic::build_state_tree(
                    screen.cols,
                    screen.rows,
                    &sem.regions,
                    &sem.controls,
                    &sem.focus,
                    &sem.components,
                );
                ok(json!({ "tree": tree, "rendered": tree.render() }))
            }
            // Wave C (items 16-30): the general SemanticNode tree — one node
            // type for regions, controls, widget internals (table rows/cells,
            // tree items, scroll edges), hyperlinks, and help hints, with
            // modal layering and provenance-tracked enabled state.
            "nodes" => {
                let tree = semantic::build_tree(&screen);
                ok(json!({
                    "tree": tree,
                    "rendered": tree.render(),
                    "layers": tree.layers,
                }))
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
            // Incremental read (Wave B item 13): return only what changed
            // since the named consumer's cursor, with dirty rows — far more
            // token-efficient than full-screen rereads for monitoring.
            "changes" => {
                let consumer = p.consumer.clone().unwrap_or_else(|| "hermes".to_string());
                let batch = sess.events_for_consumer(&consumer);
                ok(json!({
                    "consumer": consumer,
                    "cursor": batch.cursor,
                    "gap": batch.gap,
                    "first_available": batch.first_available,
                    "events": batch.events,
                    "note": if batch.events.is_empty() {
                        Some("no changes since cursor".to_string())
                    } else {
                        None
                    },
                }))
            }
            // Honest unsupported (re-review Part VIII): an empty scrollback
            // array is ambiguous with "supported but empty"; until real
            // scrollback exists, say so explicitly. `history` likewise — a
            // structure hash is not history.
            "scrollback" => err(
                ErrorCategory::Unsupported,
                "scrollback is not implemented in this backend (capability reports scrollback=false); use mode=screen for the live viewport",
            ),
            "history" => err(
                ErrorCategory::Unsupported,
                "run/session event history is not implemented; the current frame identity is available via mode=summary (structure_hash)",
            ),
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
    pub async fn tui_act(&self, p: Parameters<TuiActRequest>) -> String {
        let p = p.0;
        let mut mgr = self.manager.lock().unwrap();
        let sess = match mgr.resolve_mut(p.id()) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
        };
        // The typed action (Wave-2 item 10): built once from the request,
        // executed, and stored on the transaction for lossless replay.
        let action = match crate::execution::CanonicalAction::from_request(&p) {
            Ok(a) => a,
            Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
        };
        // Visibility policy (leak fix): sensitive payloads execute normally
        // but are redacted in EVERY recorder — the cast (via send_unrecorded),
        // the run ledger (via PersistedAction), and scenario recordings
        // (payload stripped below).
        let sensitive = p.sensitive();
        let visibility = if sensitive {
            crate::execution::InputVisibility::Sensitive
        } else {
            crate::execution::InputVisibility::Normal
        };
        // The one canonical executor (re-review item 4): anchored settle wait
        // (item 8) + honest settle reporting (item 9).
        let quiet = p.wait_ms().unwrap_or(150);
        let tx = match crate::execution::execute_act_with_visibility(
            sess,
            &action,
            quiet,
            quiet.saturating_add(1000),
            p.no_wait(),
            visibility,
        ) {
            Ok(t) => t,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        // Scenario recording in progress? Append this act (audit: scenarios
        // capture real tool traffic; sensitive payloads are not recorded —
        // and for a sensitive step the *params* are stripped to a redacted
        // placeholder so replay still works shape-wise without the secret).
        // Scoped to the resolved session generation (re-review item 5): a
        // recording for session A never absorbs session B's traffic.
        let frame_refs = {
            let (sid, gen) = (sess.id.clone(), sess.generation);
            let mut run = self.run.lock().unwrap();
            // Citable frame identities (Wave B item 11): both frames get
            // per-run ids and full provenance.
            let b = run.register_frame(&mut {
                let mut f = tx.before_frame.clone();
                f.session_id = Some(sid.clone());
                f.generation = Some(gen);
                f
            });
            let a = run.register_frame(&mut {
                let mut f = tx.after_frame.clone();
                f.session_id = Some(sid.clone());
                f.generation = Some(gen);
                f
            });
            // Run ledger (Wave-2 item 15): the reconstructable transaction
            // record, not just a counter. The ledger projects the action
            // through the visibility policy — sensitive payloads are stored
            // as Redacted(kind, byte_len), never verbatim.
            run.record_interaction(&sid, &tx);
            // Scenario capture (see block comment above): sensitive steps
            // are recorded as redacted placeholders, never with the payload.
            if sensitive {
                run.record_scenario_act(
                    &sid,
                    gen,
                    json!({
                        "kind": tx.name(),
                        "sensitive": true,
                        "redacted": true,
                        "payload_bytes": tx.canonical().payload_len(),
                    }),
                );
            } else {
                run.record_scenario_act(&sid, gen, serde_json::to_value(&p).unwrap_or_default());
            }
            serde_json::json!({ "before": format!("frame:{b}"), "after": format!("frame:{a}") })
        };
        ok(json!({
            "action": tx.name(),
            "settled": tx.settled(),
            "settle_status": tx.settle,
            "settle_reason": tx.settle_reason(),
            "elapsed_ms": tx.elapsed_ms,
            "frames": frame_refs,
            "warnings": if tx.settled() { Vec::<String>::new() } else if tx.settle == crate::execution::SettleStatus::Skipped {
                vec!["settlement was not tested (no_wait=true); reported honestly as skipped".to_string()]
            } else {
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
        // Go through the canonical wait executor (re-review P0).
        match crate::execution::execute_wait(sess, cond, p.budget_ms.unwrap_or(5000)) {
            Ok(out) => {
                // Scoped to the resolved session generation (item 5).
                {
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    let mut run = self.run.lock().unwrap();
                    run.record_event(&sid, "wait");
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
        // Wave E: `assertion: "oracle"` evaluates a declarative oracle
        // expression (shared language with contracts and audits). Any other
        // assertion name goes through the canonical assert executor.
        if p.assertion == "oracle" {
            let Some(expr) = p.text.as_deref().or(p.reference.as_deref()) else {
                return err(
                    ErrorCategory::InvalidRequest,
                    "oracle assertion requires the expression in 'text' (e.g. text: \"modal_open()\")",
                );
            };
            let sem = semantic::analyze(&screen);
            let outcome = crate::design::eval_static(expr, &screen, &sem);
            {
                // Scoped to the resolved session generation (item 5).
                let (sid, gen) = (sess.id.clone(), sess.generation);
                let mut run = self.run.lock().unwrap();
                run.record_scenario_assert(
                    &sid,
                    gen,
                    serde_json::json!({ "assertion": "oracle", "text": expr }),
                );
            }
            if outcome.passed {
                ok(json!({
                    "passed": true,
                    "assertion": "oracle",
                    "expression": expr,
                    "detail": outcome.detail,
                    "flavor": outcome.flavor,
                }))
            } else if outcome.parse_error.is_some() {
                // A malformed expression is a caller error, not a UI failure.
                err(ErrorCategory::InvalidRequest, outcome.detail)
            } else {
                err_continued(
                    ErrorCategory::AssertionFailed,
                    format!("oracle '{}' failed: {}", expr, outcome.detail),
                )
            }
        } else {
            // Go through the canonical assert executor (re-review P0).
            let (passed, detail, invalid) = crate::execution::execute_assert(&p, &screen);
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
                match run
                    .checkpoints
                    .compare(&session_id, &name, &screen, Some(&sem))
                {
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
    pub async fn tui_scenario(&self, p: Parameters<TuiScenarioParams>) -> String {
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
            "save" => {
                let name = p.name.clone().unwrap_or_else(|| "scenario".into());
                let steps = p.steps.clone().unwrap_or_default();
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
                let scenario_id = scenario.id.clone();
                let path: Option<String> = run
                    .save_scenario(scenario.clone())
                    .map(|p: std::path::PathBuf| p.to_string_lossy().to_string());
                ok(
                    json!({ "scenario_id": scenario_id, "name": name, "steps": count, "saved_to": path }),
                )
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
                    Err(e) => err(
                        ErrorCategory::InvalidRequest,
                        format!("no such scenario: {}", e),
                    ),
                }
            }
            // Replay a saved scenario against a session through the one
            // canonical executor — the regression path: record once, run
            // again later, get a real pass/fail per step.
            "run" => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "run requires 'name'"),
                };
                let scenario = match run.load_scenario(&name) {
                    Ok(sc) => sc,
                    Err(e) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("no such scenario: {}", e),
                        )
                    }
                };
                drop(run);
                let mut mgr = self.manager.lock().unwrap();
                let sess = match mgr.resolve_mut(p.id.as_deref()) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::NoSession, e.to_string()),
                };
                let report = crate::scenario::runner::ScenarioRunner::run(&scenario, sess);
                let (sid, gen) = (sess.id.clone(), sess.generation);
                drop(mgr);
                self.run
                    .lock()
                    .unwrap()
                    .record_event(&sid, &format!("scenario_run:{}", scenario.name));
                if report.steps_failed == 0 {
                    ok(json!({
                        "name": report.scenario_name,
                        "session": sid,
                        "generation": gen,
                        "passed": true,
                        "steps_total": report.steps_total,
                        "steps_passed": report.steps_passed,
                        "steps_failed": 0,
                        "step_results": report.step_results,
                    }))
                } else {
                    // Real regression: envelope stays success (transport ok),
                    // payload reports the failure honestly.
                    ok(json!({
                        "name": report.scenario_name,
                        "session": sid,
                        "generation": gen,
                        "passed": false,
                        "steps_total": report.steps_total,
                        "steps_passed": report.steps_passed,
                        "steps_failed": report.steps_failed,
                        "step_results": report.step_results,
                    }))
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
    pub async fn tui_record(&self, p: Parameters<TuiRecordParams>) -> String {
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
                let size = body.len() as u64;
                let (path, artifact) = {
                    let mut run = self.run.lock().unwrap();
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
                                    );
                                    (Some(file.to_string_lossy().to_string()), Some(r))
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
                            );
                            (None, Some(r))
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

    /// Wave D item 38: when an exploration report recorded a crash exit,
    /// run the full minimization pipeline — clean restart, delta-debug
    /// replay, saved Scenario — and emit a Finding with
    /// `reproduction = scenario_id` into the run. Returns the pipeline
    /// record for the tool response (`null` when nothing crashed).
    fn minimize_crash_finding(
        &self,
        sess: &mut crate::session::state::Session,
        seed: u64,
        report: &crate::exploration::random::ExploreReport,
    ) -> serde_json::Value {
        use crate::exploration::repro::FailureKind;

        // Only a failure-classified exit is a reproduction candidate; clean
        // exits and signal-free deaths get no pipeline (and no fabricated
        // finding).
        let crash_exit = report.exits.iter().find(|e| {
            matches!(
                e.classification,
                crate::exploration::random::ExitClassification::Error
                    | crate::exploration::random::ExitClassification::Signal
            )
        });
        let Some(exit) = crash_exit else {
            return serde_json::Value::Null;
        };
        let expected = match exit.classification {
            crate::exploration::random::ExitClassification::Signal => FailureKind::Crash,
            _ => FailureKind::Crash,
        };

        // Steps up to and including the crashing action.
        let upto = exit.action_index as usize;
        let trace: Vec<crate::exploration::random::ExplorationStep> = report
            .steps
            .iter()
            .filter(|s| s.seq as usize <= upto)
            .cloned()
            .collect();

        let name = format!("seed{seed}-act{}", exit.action_index);
        let pipeline = crate::exploration::repro::minimize_crash(sess, &trace, expected, &name);

        if !pipeline.reproduced {
            // Honest outcome: the trace did not reproduce on a clean
            // restart. Report the attempt; emit no reproduction finding.
            let mut run = self.run.lock().unwrap();
            run.extend_findings(vec![crate::audit::Finding {
                id: format!("EXPLORE-CRASH-{}", exit.action_index),
                severity: "warn".into(),
                category: "exploration".into(),
                summary: format!(
                    "Exploration crash at action {} ({}): original trace ({} steps) did not reproduce on a clean restart — not minimized, no scenario fabricated",
                    exit.action_index,
                    exit.action_name,
                    pipeline.original_len,
                ),
                evidence: vec![crate::audit::EvidenceRef::point(
                    crate::audit::EvidenceKind::Other,
                    format!("crash_at_{}", exit.action_index),
                    "process died during seeded exploration",
                )
                .with_detail(json!({
                    "seed": seed,
                    "action_index": exit.action_index,
                    "action_name": exit.action_name,
                    "exit_code": exit.exit_code,
                    "exit_signal": exit.exit_signal,
                    "attempts": pipeline.attempts,
                }))],
                confidence: 0.9,
                reproduction: None,
            }]);
            return json!({
                "reproduced": false,
                "attempts": pipeline.attempts,
                "note": "trace did not reproduce on a clean restart; no scenario fabricated",
            });
        }

        // Save the minimized scenario into the run and attach its ID to the
        // finding (item 38, the last mile). The pipeline hands back the
        // fully-built Scenario — no lossy rebuild.
        let scenario = match pipeline.scenario.clone() {
            Some(s) => s,
            None => return json!({ "reproduced": false, "attempts": pipeline.attempts }),
        };
        let scenario_id = scenario.id.clone();
        let saved = {
            let mut run = self.run.lock().unwrap();
            run.save_scenario(scenario);
            run.extend_findings(vec![crate::audit::Finding {
                id: format!("EXPLORE-CRASH-{}", exit.action_index),
                severity: "error".into(),
                category: "exploration".into(),
                summary: format!(
                    "Exploration crash at action {} ({}): minimized to {} step(s), saved as scenario {}",
                    exit.action_index,
                    exit.action_name,
                    pipeline.minimized_len,
                    scenario_id,
                ),
                evidence: vec![crate::audit::EvidenceRef::point(
                    crate::audit::EvidenceKind::Other,
                    format!("repro_{}", scenario_id),
                    "minimized reproduction saved as a replayable scenario",
                )
                .with_detail(json!({
                    "seed": seed,
                    "action_index": exit.action_index,
                    "action_name": exit.action_name,
                    "original_len": pipeline.original_len,
                    "minimized_len": pipeline.minimized_len,
                    "minimized_steps": pipeline.steps,
                    "attempts": pipeline.attempts,
                }))],
                confidence: 1.0,
                reproduction: Some(scenario_id.clone()),
            }]);
            scenario_id.clone()
        };
        json!({
            "reproduced": true,
            "scenario_id": saved,
            "original_len": pipeline.original_len,
            "minimized_len": pipeline.minimized_len,
            "minimized_steps": pipeline.steps,
            "attempts": pipeline.attempts,
        })
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
                // Novel action candidates for Hermes to choose (spec 4.3),
                // Wave D item 34: every reason is evidential. The candidate
                // context carries the run's state graph, the current state's
                // layered identity, the actions actually executed this run,
                // the coverage set, and the risk allowance.
                let screen = match sess.observe(40) {
                    Ok(s) => s,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let sem = semantic::analyze(&screen);
                let identity =
                    crate::exploration::state_graph::StateIdentity::with_semantic(&screen, &sem);
                let current = identity.id();
                let candidate_list = {
                    let run = self.run.lock().unwrap();
                    let action_history: Vec<String> = run
                        .transactions()
                        .iter()
                        .map(|t| t.action.clone())
                        .collect();
                    let coverage: Vec<String> = run.focus_graph.nodes.keys().cloned().collect();
                    // Item 49: a loaded contract feeds the `contract` evidence
                    // source — declared keys never exercised become candidates
                    // citing the contract, not guesses.
                    let contract = run.contract();
                    let ctx = crate::exploration::candidates::CandidateContext {
                        state_graph: &run.state_graph,
                        current,
                        action_history: &action_history,
                        coverage: &coverage,
                        contract,
                        allowed_risk: p
                            .max_risk
                            .as_deref()
                            .and_then(crate::intent::ActionRisk::parse)
                            .unwrap_or(crate::intent::ActionRisk::Mutating),
                    };
                    crate::exploration::candidates::suggest(&screen, &sem, &ctx)
                };
                ok(json!({
                    "novel_actions": candidate_list,
                    "state": identity.id().as_str(),
                }))
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
                        // Wave D item 38: a crash exit gets the full
                        // minimization pipeline — clean restart, delta-debug
                        // replay, saved Scenario, Finding with
                        // reproduction=scenario_id.
                        let repro = self.minimize_crash_finding(sess, seed, &report);
                        ok(json!({
                            "seed": seed,
                            "report": report,
                            "state_graph": graph_summary,
                            "state_graph_path": graph_path,
                            "reproduction": repro,
                        }))
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            "semantic" => {
                // Wave D item 35: screen-reading exploration. Picks the
                // top evidential candidate each round (affordances, untried
                // keys from the graph, unreached controls) and executes it
                // through the canonical executor. Focus edges land in the
                // run's ID-keyed FocusGraph with the acting key as via.
                let max_risk = p
                    .max_risk
                    .as_deref()
                    .and_then(crate::intent::ActionRisk::parse)
                    .unwrap_or(crate::intent::ActionRisk::Mutating);
                let (graph_budget, run_graph_summary, contract) = {
                    let run = self.run.lock().unwrap();
                    (
                        run.state_graph.budget().clone(),
                        (run.state_graph.state_count(), run.state_graph.transition_count()),
                        run.contract().cloned(),
                    )
                };
                let max_actions = p.actions.unwrap_or(20);
                // Local graphs during the loop (session I/O must not hold
                // the run lock); merged into the run after.
                let mut local_graph = crate::exploration::state_graph::StateGraph::new(
                    graph_budget.clone(),
                );
                let mut focus_graph = crate::semantic::focus_graph::FocusGraph::new();
                let report = match crate::exploration::semantic::run_with_contract(
                    sess,
                    &mut local_graph,
                    &mut focus_graph,
                    &graph_budget,
                    max_actions,
                    max_risk,
                    contract.as_ref(),
                ) {
                    Ok(r) => r,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                // Merge what actually happened into the run's graphs.
                {
                    let mut run = self.run.lock().unwrap();
                    run.state_graph.merge(&local_graph);
                    run.focus_graph.merge(&focus_graph);
                }
                ok(json!({
                    "mode": "semantic",
                    "max_risk": max_risk.name(),
                    "report": report,
                    "focus_graph": focus_graph.summary(),
                    "run_graph_before": {
                        "states": run_graph_summary.0,
                        "transitions": run_graph_summary.1,
                    },
                }))
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
                    // The ID-keyed focus graph (Wave D item 36): Tab order
                    // and reverse-traversal proof, accumulated across every
                    // observe and audit in this run.
                    "focus_graph": run.focus_graph.summary(),
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

        // The audit ENGINE owns the static-vs-active decision (re-review
        // P0 fix 2): `full` is the composite (static + every active driver),
        // and no profile is weaker than its members. The MCP layer only
        // surfaces the resulting mode. Item 49: the loaded contract rides
        // along for profile=contract.
        let report = {
            let contract = self.run.lock().unwrap().contract().cloned();
            match crate::audit::orchestrator::run_profile_with_contract(
                sess,
                &profile,
                contract.as_ref(),
            ) {
                Ok(r) => r,
                Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
            }
        };

        // Findings accumulate in the run context (composition root) so a
        // later audit/coverage query can see prior evidence. Driven focus
        // edges merge into the run's persistent ID-keyed FocusGraph (Wave D
        // item 36), so multiple audits accumulate traversal proof.
        let focus_summary = {
            let mut run = self.run.lock().unwrap();
            run.focus_graph.merge(&report.focus_graph);
            run.extend_findings(report.findings.clone());
            json!({
                "nodes": run.focus_graph.nodes.len(),
                "edges": run.focus_graph.edges.len(),
                "tab_cycle": run.focus_graph.tab_cycle(),
                "reverse_tab_gaps": run.focus_graph.reverse_tab_gaps(),
            })
        };
        ok(json!({
            "profile": profile,
            "mode": report.mode,
            "finding_count": report.findings.len(),
            "findings": report.findings,
            "focus_graph": focus_summary,
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
    pub async fn tui_run(&self, p: Parameters<TuiRunParams>) -> String {
        let p = p.0;
        match p.action.as_str() {
            "status" => {
                let sessions = self.manager.lock().unwrap().list();
                ok(self.run.lock().unwrap().status(
                    sessions
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
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
                // Drain terminal-event queues into the run (Wave B item 14)
                // before the flush so the event logs land in the artifacts.
                // The id list is bound FIRST: a `for sid in lock().list()`
                // would keep the manager guard alive for the whole loop and
                // re-locking it below self-deadlocks (std Mutex is not
                // reentrant) — the exact hang this replaces.
                let session_ids = self.manager.lock().unwrap().list();
                for sid in session_ids {
                    if let Ok(sess) = self.manager.lock().unwrap().resolve_mut(Some(&sid)) {
                        run.hold_events(&sid, sess.drain_events());
                    }
                }
                let already = run.is_closed();
                let sessions = self.manager.lock().unwrap().list();
                let summary = run.status(
                    sessions
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                );
                let kill = p.kill_sessions.unwrap_or(false);
                let result = run.close();
                drop(run);
                if let Err(e) = result {
                    return err(
                        ErrorCategory::InternalError,
                        format!("close flush failed: {e}"),
                    );
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

    /// Contracts (Wave E items 39–49): load a design contract, validate the
    /// document, check the running app's conformance (PASS/FAIL/WARN), and
    /// compare conformance across points in time. Loading a contract also
    /// installs its `volatile_patterns` into the session's normalization
    /// policy (item 48) and arms contract-guided exploration (item 49).
    #[tool(
        name = "tui_contract",
        description = "Design contracts: load, validate, check conformance (PASS/FAIL/WARN against the running app), and compare runs."
    )]
    pub async fn tui_contract(&self, p: Parameters<TuiContractParams>) -> String {
        let p = p.0;
        match p.action.as_str() {
            // ── load: parse + validate + remember + apply policy ──
            "load" => {
                let Some(path) = p.path.clone() else {
                    return err(ErrorCategory::InvalidRequest, "load requires 'path'");
                };
                let contract = match crate::design::load_design_contract(std::path::Path::new(&path))
                {
                    Ok(c) => c,
                    Err(e) => return err(ErrorCategory::InvalidRequest, e),
                };
                // Apply the contract's normalization policy to every live
                // session (item 48): subsequent structure hashes collapse the
                // declared volatile patterns.
                let policy = match contract.normalization_policy() {
                    Ok(pol) => std::sync::Arc::new(pol),
                    Err(e) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("contract volatile_patterns invalid: {e}"),
                        )
                    }
                };
                let applied: Vec<String> = {
                    let mut mgr = self.manager.lock().unwrap();
                    let ids = mgr.list();
                    for sid in &ids {
                        if let Ok(sess) = mgr.get_mut(sid) {
                            sess.set_normalization_policy(policy.clone());
                        }
                    }
                    ids
                };
                let summary = {
                    let mut run = self.run.lock().unwrap();
                    run.set_contract(contract.clone(), path.clone());
                    json!({
                        "name": contract.schema.name,
                        "version": contract.schema.version,
                        "viewports": contract.viewports.len(),
                        "components": contract.components.len(),
                        "interactions": contract.interactions.len(),
                        "layout_constraints": contract.layout.len(),
                        "oracles": contract.oracles.len(),
                        "volatile_patterns": contract.volatile_patterns.len(),
                        "policy_applied_to_sessions": applied,
                    })
                };
                ok(json!({ "loaded": true, "path": path, "contract": summary }))
            }
            // ── validate: document-only, no session needed ──
            "validate" => {
                let Some(path) = p.path.clone() else {
                    return err(ErrorCategory::InvalidRequest, "validate requires 'path'");
                };
                // Parse WITHOUT the load-time hard stop, so a report of every
                // problem comes back instead of only the first.
                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => return err(ErrorCategory::InvalidRequest, format!("cannot read: {e}")),
                };
                let parsed: Result<crate::design::ProjectContract, _> = if path.ends_with(".json") {
                    serde_json::from_str(&content)
                        .map_err(|e| format!("Failed to parse design contract JSON: {e}"))
                } else {
                    serde_yaml::from_str(&content)
                        .map_err(|e| format!("Failed to parse design contract YAML: {e}"))
                };
                match parsed {
                    Err(e) => err(ErrorCategory::InvalidRequest, e),
                    Ok(contract) => {
                        let results = crate::design::conformance::validate_document(&contract);
                        let verdict = results.iter().fold(
                            crate::design::Verdict::Pass,
                            |acc, r| acc.merge(r.verdict),
                        );
                        ok(json!({
                            "contract": contract.schema.name,
                            "version": contract.schema.version,
                            "verdict": verdict.as_str(),
                            "results": results,
                        }))
                    }
                }
            }
            // ── status: full conformance check against the running app ──
            "status" => {
                let contract = {
                    let run = self.run.lock().unwrap();
                    match run.contract() {
                        Some(c) => c.clone(),
                        None => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                "no contract loaded; call tui_contract action=load path=... first",
                            )
                        }
                    }
                };
                self.check_contract_against(p.id.as_deref(), &contract)
            }
            // ── compare: run conformance now, diff against the baseline ──
            "compare" => {
                let contract = {
                    let run = self.run.lock().unwrap();
                    match run.contract() {
                        Some(c) => c.clone(),
                        None => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                "no contract loaded; call tui_contract action=load path=... first",
                            )
                        }
                    }
                };
                let current = match self.check_contract_inner(p.id.as_deref(), &contract) {
                    Ok(r) => r,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let label = p.label.clone().unwrap_or_else(|| "current".into());
                let mut run = self.run.lock().unwrap();
                let baseline_label = p.baseline.clone().unwrap_or_else(|| "baseline".into());
                let baseline = run
                    .contract_baselines()
                    .get(&baseline_label)
                    .cloned();
                // Record the current result under its label for future compares.
                run.record_contract_baseline(&label, &current);
                match baseline {
                    None => ok(json!({
                        "baseline": baseline_label,
                        "baseline_available": false,
                        "note": format!("no baseline '{baseline_label}' recorded yet; this run's result is now stored as '{label}' — fix what you need to fix, then compare again"),
                        "current": current.summary(),
                        "current_results": current.results,
                        "regressions": [],
                        "fixed": [],
                    })),
                    Some(base) => {
                        let (regressions, fixed) = diff_contract_reports(&base, &current);
                        let verdict = if !regressions.is_empty() {
                            crate::design::Verdict::Fail
                        } else {
                            current.verdict
                        };
                        // Failed comparisons become findings (item 49).
                        if !regressions.is_empty() {
                            let findings: Vec<crate::audit::Finding> = regressions
                                .iter()
                                .map(|(name, before, after)| crate::audit::Finding {
                                    id: "CONTRACT-REGRESSION".into(),
                                    severity: "error".into(),
                                    category: "contract/compare".into(),
                                    summary: format!(
                                        "{name}: was {} ({}), now {} ({})",
                                        before.verdict.as_str(),
                                        before.detail,
                                        after.verdict.as_str(),
                                        after.detail
                                    ),
                                    evidence: vec![crate::audit::EvidenceRef::point(
                                        crate::audit::EvidenceKind::Other,
                                        "contract_compare",
                                        name.clone(),
                                    )
                                    .with_detail(json!({
                                        "contract": contract.schema.name,
                                        "baseline": baseline_label,
                                        "check": name,
                                        "before": before.detail,
                                        "after": after.detail,
                                    }))],
                                    confidence: 1.0,
                                    reproduction: None,
                                })
                                .collect();
                            run.extend_findings(findings);
                        }
                        ok(json!({
                            "baseline": baseline_label,
                            "baseline_available": true,
                            "current": current.summary(),
                            "baseline_summary": base.summary(),
                            "verdict": verdict.as_str(),
                            "regressions": regressions.iter().map(|(n, b, a)| json!({
                                "check": n,
                                "before": b.verdict.as_str(),
                                "before_detail": b.detail,
                                "after": a.verdict.as_str(),
                                "after_detail": a.detail,
                            })).collect::<Vec<_>>(),
                            "fixed": fixed.iter().map(|(n, b, a)| json!({
                                "check": n,
                                "before": b.verdict.as_str(),
                                "after": a.verdict.as_str(),
                                "after_detail": a.detail,
                            })).collect::<Vec<_>>(),
                        }))
                    }
                }
            }
            other => err(
                ErrorCategory::InvalidRequest,
                format!("unknown contract action '{}' (load|validate|status|compare)", other),
            ),
        }
    }

    /// Lock-and-resolve wrapper around [`Self::check_contract_inner`].
    fn check_contract_against(
        &self,
        id: Option<&str>,
        contract: &crate::design::ProjectContract,
    ) -> String {
        match self.check_contract_inner(id, contract) {
            Ok(report) => {
                // Findings feed the run ledger (item 49).
                let findings = report.findings();
                let summary = report.summary();
                let results = report.results.clone();
                let mut run = self.run.lock().unwrap();
                run.extend_findings(findings);
                run.record_contract_baseline("baseline", &report);
                ok(json!({
                    "verdict": report.verdict.as_str(),
                    "summary": summary,
                    "results": results,
                }))
            }
            Err(e) => err(ErrorCategory::BackendError, e.to_string()),
        }
    }

    fn check_contract_inner(
        &self,
        id: Option<&str>,
        contract: &crate::design::ProjectContract,
    ) -> anyhow::Result<crate::design::ContractReport> {
        let mut mgr = self.manager.lock().unwrap();
        let sess = mgr.resolve_mut(id)?;
        crate::design::check_contract(sess, contract)
    }
}

/// Diff two contract reports result-by-result, keyed by `group + name`.
/// Regressions: Pass→Fail (and Pass→Warn for required checks). Fixed:
/// Fail→Pass, Warn→Pass. Verdict-neutral changes (Warn→Fail on optional
/// checks etc.) are reported as regressions too — stricter is a regression
/// whenever the check was required.
/// One (key, before, after) row per changed check in a contract comparison.
type ContractCheckDiff = Vec<(String, crate::design::CheckResult, crate::design::CheckResult)>;

fn diff_contract_reports(
    base: &crate::design::ContractReport,
    current: &crate::design::ContractReport,
) -> (ContractCheckDiff, ContractCheckDiff) {
    use crate::design::{CheckResult, Verdict};
    let key = |r: &CheckResult| format!("{}/{}", r.group, r.name);
    let mut regressions = Vec::new();
    let mut fixed = Vec::new();
    for cur in &current.results {
        let Some(prev) = base.results.iter().find(|r| key(r) == key(cur)) else {
            continue; // new check, no history
        };
        let worsened = match (prev.verdict, cur.verdict) {
            (Verdict::Pass, Verdict::Fail) => true,
            (Verdict::Pass, Verdict::Warn) => cur.required,
            (Verdict::Warn, Verdict::Fail) => cur.required,
            _ => false,
        };
        let improved = matches!(
            (prev.verdict, cur.verdict),
            (Verdict::Fail, Verdict::Pass) | (Verdict::Warn, Verdict::Pass) | (Verdict::Fail, Verdict::Warn)
        );
        if worsened {
            regressions.push((key(cur), prev.clone(), cur.clone()));
        } else if improved {
            fixed.push((key(cur), prev.clone(), cur.clone()));
        }
    }
    (regressions, fixed)
}

// Generate `call_tool`/`list_tools`/`get_info` from the tool router above.
#[rmcp::tool_handler]
impl ServerHandler for TuiLabServer {}
