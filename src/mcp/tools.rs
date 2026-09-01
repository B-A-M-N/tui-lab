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
    build_wait, err, err_continued, err_invalid_selector, lease_refused, ok,
};
use crate::mcp::params::*;
use crate::screen::diff;
use crate::semantic;
use crate::session::SessionPool;
use rmcp::handler::server::wrapper::Parameters;

/// Shared MCP state: the session pool (Wave G item 73 — per-session actors;
/// no global blocking lock) plus the run context (composition root for
/// checkpoints, scenarios, recordings, findings, exploration graph — audit
/// item: "Run/Artifact Model").
///
/// Lock discipline: the run Mutex is short-lock and never held across an
/// actor `.await`. Handlers ship one closure per session operation; the
/// closure may lock the run ON THE ACTOR THREAD (a sync thread, so a std
/// lock there blocks nothing async).
#[derive(Clone)]
pub struct TuiLabServer {
    sessions: Arc<SessionPool>,
    run: Arc<std::sync::Mutex<crate::run::RunContext>>,
}

/// Which named view of a session a `tui://sessions/<id>/<view>` resource
/// resolves to. `TerminalProfile` is observationally pure — it never forces a
/// screen settle, unlike the screen-backed views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionView {
    TerminalProfile,
    Semantic,
    Screen,
}

impl TuiLabServer {
    pub fn new() -> Self {
        TuiLabServer {
            sessions: Arc::new(SessionPool::new()),
            run: Arc::new(std::sync::Mutex::new(crate::run::RunContext::ephemeral())),
        }
    }

    /// Resolve a `tui://` resource URI to its text content (Wave G item
    /// 72). Unknown schemes/ids are honest `resource_not_found` errors that
    /// name what WAS accepted, so a stale id can be self-corrected.
    async fn resolve_resource(&self, uri: &str) -> Result<String, rmcp::model::ErrorData> {
        use crate::semantic;
        let not_found = |msg: String| rmcp::model::ErrorData::resource_not_found(msg, None);
        if uri == "tui://findings" {
            let run = self.run.lock().unwrap();
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "run": run.id,
                "findings": run.findings(),
                "count": run.findings().len(),
            }))
            .unwrap_or_default());
        }
        if let Some(rest) = uri.strip_prefix("tui://runs/") {
            let rest = rest.trim_end_matches('/');
            if rest.is_empty() {
                return Err(not_found("empty run id".to_string()));
            }
            // The live run first (it carries live session state)…
            {
                let run = self.run.lock().unwrap();
                if rest == run.id {
                    let sessions = self.sessions.list();
                    return Ok(run
                        .status(
                            sessions
                                .into_iter()
                                .map(serde_json::Value::String)
                                .collect(),
                        )
                        .to_string());
                }
            }
            // …then any persisted run on disk (Wave G item 75: the browser
            // reads closed runs without resuming them). Search roots: the
            // base configured for the live run (its primary session cwd or
            // durable root's parent), so `tui://runs/<old-id>` resolves
            // against where runs actually live for this workspace.
            let mut bases = self.run.lock().unwrap().browser_bases();
            // The server's own working directory is the workspace anchor:
            // the canonical browser workflow is a restarted server sitting
            // in the same repo where runs were persisted.
            if let Ok(cwd) = std::env::current_dir() {
                if !bases.contains(&cwd) {
                    bases.push(cwd);
                }
            }
            for base in bases {
                if let Some(dir) = crate::run::RunContext::resolve_run_dir(&base, rest) {
                    let restored = crate::run::RunContext::restore(&dir)
                        .map_err(|e| not_found(format!("run '{rest}' unreadable: {e}")))?;
                    let mut summary = restored.status(Vec::new());
                    summary["live"] = serde_json::Value::Bool(false);
                    return Ok(summary.to_string());
                }
            }
            let live_id = self.run.lock().unwrap().id.clone();
            return Err(not_found(format!(
                "no run '{rest}' in this server or under its runs roots (this server's live run is '{live_id}'; use tui_run action=list to see persisted runs)"
            )));
        }
        // tui://sessions/<id>/semantic | tui://sessions/<id>/screen
        if let Some(rest) = uri.strip_prefix("tui://sessions/") {
            let (sid, view) = match rest.split_once('/') {
                Some((sid, view)) => (sid, view.trim_end_matches('/')),
                None => (rest, ""),
            };
            let session_view = match view {
                // TerminalProfile needs no screen: it reads the live backend
                // capabilities without triggering an observation.
                "terminal-profile" => SessionView::TerminalProfile,
                "semantic" => SessionView::Semantic,
                "screen" => SessionView::Screen,
                other => {
                    return Err(not_found(format!(
                        "unknown session resource view '{other}' (expected \
                         'terminal-profile', 'semantic', or 'screen')"
                    )))
                }
            };
            let selector = sid.to_string();
            let in_job = selector.clone();
            let snapshot = self
                .sessions
                .with_session(Some(&selector), move |s| {
                    let selector = in_job;
                    if let SessionView::TerminalProfile = session_view {
                        // Evidence-backed capability report, no screen settle.
                        let profile = s.terminal_profile();
                        return Some(
                            serde_json::to_string_pretty(&profile).unwrap_or_default(),
                        );
                    }
                    // Passive resource read: consume the latest COMMITTED
                    // frame without triggering a settle cycle and WITHOUT
                    // advancing the session-global previous/current baseline.
                    // The explicit `observe()` path (tui_observe) is the only
                    // thing that should move a consumer's diff cursor; a peek
                    // at the screen must be observationally pure. We only
                    // settle once to establish a first frame if none exists
                    // (a freshly-started session that was never observed).
                    let screen = match s.last() {
                        Some(f) => f.clone(),
                        None => s.observe(40).ok()?,
                    };
                    if matches!(session_view, SessionView::Semantic) {
                        // Fused truth: the resource serves the SAME analysis
                        // observe modes see — cached detection + native
                        // overlay — never an inference-only view.
                        match s.fused_frame() {
                            Some((sem, _tree, _report)) => {
                                Some(serde_json::to_string_pretty(&sem).unwrap_or_default())
                            }
                            None => {
                                let sem = semantic::analyze(&screen);
                                Some(serde_json::to_string_pretty(&sem).unwrap_or_default())
                            }
                        }
                    } else {
                        Some(
                            serde_json::to_string_pretty(&serde_json::json!({
                                "session": selector,
                                "cols": screen.cols,
                                "rows": screen.rows,
                                "title": screen.title,
                                "cursor": screen.cursor,
                                "viewport_text": screen.viewport_text,
                                "structure_hash": screen.structure_hash,
                                "visual_hash": screen.visual_hash,
                                "process": screen.process,
                            }))
                            .unwrap_or_default(),
                        )
                    }
                })
                .await;
            return match snapshot {
                Ok(Some(text)) => Ok(text),
                Ok(None) => Err(not_found(format!(
                    "session '{sid}' could not be observed (stopped or exited)"
                ))),
                Err(e) => Err(not_found(e.to_string())),
            };
        }
        Err(not_found(format!(
            "unknown resource URI '{uri}' (templates: tui://runs/{{run_id}}, \
             tui://sessions/{{session_id}}/semantic, tui://sessions/{{session_id}}/screen, \
             tui://findings)"
        )))
    }

    /// Run a closure against one session inside its actor, mapping actor
    /// failures to the envelope error channel. The closure runs to
    /// completion on the session's own thread.
    async fn with_sess<R, F>(
        &self,
        id: Option<&str>,
        job: F,
    ) -> Result<R, rmcp::model::CallToolResult>
    where
        R: Send + 'static,
        F: FnOnce(&mut crate::session::Session) -> R + Send + 'static,
    {
        self.sessions
            .with_session(id, job)
            .await
            .map_err(|e| err(e.category(), e.to_string()))
    }
}

impl Default for TuiLabServer {
    fn default() -> Self {
        TuiLabServer::new()
    }
}

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl TuiLabServer {
    /// Session lifecycle: start / restart / stop / list / status.
    #[tool(
        name = "tui_session",
        description = "Manage TUI sessions: start, restart, stop, list, status, plus the human control lease (lease/release)."
    )]
    pub async fn tui_session(
        &self,
        p: Parameters<TuiSessionParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::SessionAction as A;
        let Some(action) = p.action.known() else {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown session action '{}' (expected one of: {})",
                    match &p.action {
                        crate::mcp::params::Known::Other(s) => s.clone(),
                        _ => String::new(),
                    },
                    <A as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        };
        match action {
            A::Start => {
                let cols = p.cols.unwrap_or(80);
                let rows = p.rows.unwrap_or(24);
                let command = match &p.command {
                    Some(c) => c.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "start requires 'command'"),
                };
                // Honest backend/isolation negotiation (spec section 34). Do not
                // silently start portable-pty when an unsupported backend is asked.
                // Wave F items 50–51: `cli` selects the line CLI engine for
                // non-screen targets (git/npm/pytest-style output).
                let backend = p.backend.clone().unwrap_or_else(|| "auto".into());
                // Wave G item 77: typed isolation profile; the enum converts
                // into the engine-level Isolation.
                let isolation_param =
                    p.isolation
                        .clone()
                        .unwrap_or(crate::mcp::params::Known::Known(
                            crate::mcp::params::IsolationParam::Local,
                        ));
                let isolation: crate::session::isolation::Isolation = match &isolation_param {
                    crate::mcp::params::Known::Known(ip) => (*ip).into(),
                    crate::mcp::params::Known::Other(other) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "unknown isolation profile '{}' (supported: local, clean, strict)",
                                other
                            ),
                        )
                    }
                };
                match backend.as_str() {
                    "auto" | "portable_vt100" | "cli" | "line_cli" => {}
                    "tui_test" => {
                        return err(
                            ErrorCategory::Unsupported,
                            "backend 'tui_test' is not wired in this build; only 'auto'/'portable_vt100'/'cli' are supported",
                        )
                    }
                    other => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "unknown backend '{}' (supported: auto, portable_vt100, cli)",
                                other
                            ),
                        )
                    }
                }
                let env: Vec<(String, String)> = p.env.unwrap_or_default().into_iter().collect();
                let isolation_name = isolation.name().to_string();
                let args = p.args.unwrap_or_default();
                // Launch inside the pool; the actor owns the session from
                // birth (Wave G item 73).
                let started = self.sessions.start(
                    &command,
                    &args,
                    p.cwd.as_deref(),
                    &env,
                    cols,
                    rows,
                    &backend,
                    &isolation_name,
                );
                let id = match started.await {
                    Ok(id) => id,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                // Read back the launch facts inside the actor (the session
                // never crosses the boundary — only this summary does).
                let facts = self
                    .with_sess(Some(&id), |s| {
                        let caps = s.capabilities();
                        let launch = s.launch().cloned();
                        let version = s.backend_version();
                        let kind = s.backend_kind.clone();
                        let generation = s.generation;
                        let iso_evidence = s.isolation_evidence().cloned();
                        (caps, launch, version, kind, generation, iso_evidence)
                    })
                    .await;
                let (caps, launch, version, kind, generation, iso_evidence) = match facts {
                    Ok(f) => f,
                    Err(e) => return e,
                };
                // Attach the launch spec to the run, per session (audit
                // re-review item 1 + P0 fix 6: the run owns session/launch
                // correlation for *all* sessions, so relaunches, restarts,
                // and multi-session replay share one identity). Short lock,
                // after the actor reply — never across an await.
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
                    "isolation": iso_evidence,
                }))
            }
            A::Restart => {
                let id = match p.id.clone().or_else(|| self.sessions.active_id()) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session to restart"),
                };
                // Restart the SAME logical session: same id, next generation,
                // reusing the stored LaunchSpec (spec section 13).
                match self.sessions.restart(&id).await {
                    Ok((new_id, generation)) => {
                        let caps = self.with_sess(Some(&new_id), |s| s.capabilities()).await;
                        match caps {
                            Ok(caps) => ok(json!({
                                "session": new_id,
                                "generation": generation,
                                "restarted_from": id,
                                "capabilities": caps,
                            })),
                            Err(e) => return e,
                        }
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            A::Stop => {
                let id = match p.id.clone().or_else(|| self.sessions.active_id()) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                match self.sessions.stop(&id).await {
                    Ok(()) => ok(json!({ "stopped": id })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            A::List => ok(json!({ "sessions": self.sessions.list() })),
            // Wave G item 76: take the human control lease.
            A::Lease => {
                let id = match p.id.clone().or_else(|| self.sessions.active_id()) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                let holder = p.holder.clone().unwrap_or_else(|| "human".into());
                let ttl = p.ttl_ms.unwrap_or(300_000);
                let selector = id.clone();
                self.with_sess(Some(&selector), move |s| {
                    match s.acquire_lease(&holder, ttl) {
                        Ok(lease) => ok(json!({
                            "session": id.clone(),
                            "leased": true,
                            "holder": lease.holder,
                            "ttl_ms": lease.ttl_ms,
                            "note": "machine-driving tools (act/explore/audit/replay) refuse this session while the lease is valid; observe stays allowed",
                        })),
                        Err(existing) => ok(json!({
                            "session": id,
                            "leased": false,
                            "holder": existing.holder,
                            "remaining_ms": existing.remaining_ms(),
                            "note": "a live lease is already held; release it or wait for expiry",
                        })),
                    }
                })
                .await
                .unwrap_or_else(|e| e)
            }
            A::Release => {
                let id = match p.id.clone().or_else(|| self.sessions.active_id()) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                let selector = id.clone();
                self.with_sess(Some(&selector), move |s| {
                    let released = s.release_lease();
                    ok(json!({ "session": id.clone(), "released": released }))
                })
                .await
                .unwrap_or_else(|e| e)
            }
            A::Status => {
                let id = match p.id.clone().or_else(|| self.sessions.active_id()) {
                    Some(i) => i,
                    None => return err(ErrorCategory::NoSession, "no session"),
                };
                let selector = id.clone();
                self.with_sess(Some(&selector), move |s| {
                    let caps = s.capabilities();
                    let lease = s.active_lease();
                    let iso = s.isolation_evidence().cloned();
                    ok(json!({
                        "session": id.clone(),
                        "command": s.command,
                        "backend": s.backend_kind,
                        "capabilities": caps,
                        "process": s.process(),
                        "lease": lease.map(|l| json!({
                            "holder": l.holder,
                            "remaining_ms": l.remaining_ms(),
                        })),
                        "isolation": iso,
                    }))
                })
                .await
                .unwrap_or_else(|e| e)
            }
        }
    }

    /// Observe the screen: summary / screen / cells / region / semantic / diff / scrollback / history.
    #[tool(
        name = "tui_observe",
        description = "Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state."
    )]
    pub async fn tui_observe(
        &self,
        p: Parameters<TuiObserveParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::ObserveMode as OM;
        let mode = p
            .mode
            .clone()
            .unwrap_or(crate::mcp::params::Known::Known(OM::Summary));
        if mode.known().is_none() {
            let bad = match &mode {
                crate::mcp::params::Known::Other(o) => o.clone(),
                _ => String::new(),
            };
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown observe mode '{}' (expected one of: {})",
                    bad,
                    <OM as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
        let selector = p.id.clone();
        let run = self.run.clone();
        self.with_sess(selector.as_deref(), move |sess| {
            // Lazy screen capture (re-review item 5): only the modes that
            // render a screen need to pay a settle cycle and advance the
            // session baseline. `changes`/`scrollback`/`search`/
            // `command_state` read their own retained state instead, so they
            // cost no PTY round-trip and do NOT move `previous` — a passive
            // read must not re-anchor someone's diff baseline.
            let idle = p.idle_ms.unwrap_or(80);
            // Returns the screen, or the error that already belongs to a
            // `return err(...)` in the calling arm (the callback body is not a
            // Result, so `?` is not available — the call sites match instead).
            let sweep = |s: &mut crate::session::Session| {
                match s.observe(idle) {
                    Ok(screen) => {
                        run.lock().unwrap().bump_event();
                        Ok(screen)
                    }
                    Err(e) => Err((ErrorCategory::BackendError, e.to_string())),
                }
            };
            // First error from a lazy screen capture short-circuits the arm.
            macro_rules! cap {
                () => {
                    match sweep(sess) {
                        Ok(screen) => screen,
                        Err((c, m)) => return err(c, m),
                    }
                };
            }

            if mode.known().is_none() {
                unreachable!("validated above");
            }
        match mode.known().copied().unwrap() {
            OM::Summary => {
                let screen = cap!();
                // Compact summary for agent consumption (spec 36). Avoids
                // returning the full viewport text (which could be 160x50).
                // Use mode=screen for full text.
                //
                // Fused semantic truth (re-review Wave-4): summary reports
                // the SAME analysis every other semantic-bearing mode sees —
                // one cached detection pass + native overlay — so `focus`
                // here is the app's declared focus when it cooperates, not
                // an inference-only verdict the nodes mode contradicts.
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
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
                    let mut run = run.lock().unwrap();
                    run.record_focus_observation(
                        &sess.id,
                        prev.control.clone(),
                        focused.clone(),
                        prev.control_id.as_deref(),
                        sem.focus.control_id.as_deref(),
                        "unknown",
                    );
                    // Wave F item 64: fold any new native coverage events
                    // into the run ledger.
                    run.collect_native_coverage(&sess.id, sess.native_channel());
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
                    // Wave F item 55: OSC8 hyperlinks surfaced in the summary —
                    // the URI is the actionable fact, the span is where to click.
                    "hyperlinks": screen.hyperlinks.iter().map(|l| json!({
                        "uri": l.uri,
                        "id": l.id,
                        "start": l.start,
                        "end": l.end,
                    })).collect::<Vec<_>>(),
                    "structure_hash": screen.structure_hash,
                    "visual_hash": screen.visual_hash,
                    "cursor": screen.cursor,
                    // Wave F items 58–63: native cooperation status — the
                    // app's own view of itself, when it provides one.
                    "native": {
                        "active": sess.native_channel().latest.is_some(),
                        "framework": sess.native_channel().framework,
                    },
                }))
            }
            OM::Screen => {
                let screen = cap!();
                ok(json!({ "viewport_text": screen.viewport_text }))
            }
            OM::Cells => {
                let screen = cap!();
                ok(json!({ "cells": screen.cells }))
            }
            OM::Semantic => {
                let screen = cap!();
                // Fused truth: the flat semantic surface carries the native
                // overlay too (focus rewrite, native enable/value/label on
                // matched controls) — the same facts the nodes tree sees.
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                ok(json!({ "semantic": sem }))
            }
            // The hierarchical rendering (re-review Wave-4): regions nested
            // per containment, controls inside their regions, focus and
            // screen-level components attached. This is the shape to compare
            // against an intended design without re-deriving containment.
            // Built from the FUSED flat analysis, so a native focus
            // declaration is reflected here, not just in nodes mode.
            OM::Tree => {
                let screen = cap!();
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
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
            // Wave F (items 58–63): when the app cooperates via the native
            // side channel, its real tree is merged over inference and the
            // merge report names what matched.
            OM::Nodes => {
                let screen = cap!();
                // Fused truth: the tree comes from the same cached detection
                // pass (no second detector run) and the same native overlay
                // as every other semantic-bearing mode.
                let (sem, tree, native_report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                let _ = &sem;
                ok(json!({
                    "tree": tree,
                    "rendered": tree.render(),
                    "layers": tree.layers,
                    "native": {
                        "active": native_report.active(),
                        "framework": sess.native_channel().framework,
                        "app": sess.native_channel().app,
                        "matched": native_report.matched,
                        "native_only": native_report.native_only,
                        "frames_accepted": sess.native_channel().frames_accepted,
                        "frames_invalid": sess.native_channel().frames_invalid,
                    },
                }))
            }
            OM::Diff => {
                let screen = cap!();
                // ONE diff path (re-review item 7): the sweep above stashed
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
            OM::Changes => {
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
            // Wave F item 53: real scrollback (viewport + history). A backend
            // that cannot retain history still answers — honestly, with the
            // capability state named.
            OM::Scrollback => {
                let lines = sess
                    .backend_scrollback()
                    .map_err(|e| err(ErrorCategory::BackendError, e.to_string()))
                    .unwrap_or_default();
                let supported = sess.capabilities().scrollback;
                ok(json!({
                    "supported": supported,
                    "lines": lines,
                    "count": lines.len(),
                    "note": if supported { None } else {
                        Some("this backend retains no history; the array is empty because there is genuinely nothing to return".to_string())
                    },
                }))
            }
            // Wave F item 53: search viewport + scrollback.
            OM::Search => {
                let Some(query) = p.text.clone().or(p.query.clone()) else {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "mode=search requires 'text' (the query)",
                    );
                };
                match sess.backend_search(&query) {
                    Ok(hits) => ok(json!({
                        "query": query,
                        "hits": hits,
                        "count": hits.len(),
                    })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            // Wave F item 54: OSC 133 shell-integration state.
            OM::CommandState => {
                match sess.backend_command_state() {
                    Some(cs) => ok(json!({ "command_state": cs })),
                    None => ok(json!({
                        "command_state": null,
                        "note": "no shell integration observed (no OSC 133 traffic); command waits cannot resolve"
                    })),
                }
            }
            OM::History => err(
                ErrorCategory::Unsupported,
                "run/session event history is not implemented; the current frame identity is available via mode=summary (structure_hash)",
            ),
        }
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Drive input: key / keys / type / paste / mouse_* / resize / signal / raw.
    /// After acting, returns a BEFORE/AFTER transition (spec section 11/30).
    #[tool(
        name = "tui_act",
        description = "Drive input. Returns a screen transition after the action. Actions: key, keys, type, paste, raw, mouse_click, mouse_press, mouse_release, mouse_move, mouse_drag, mouse_scroll, resize, signal."
    )]
    pub async fn tui_act(&self, p: Parameters<TuiActRequest>) -> rmcp::model::CallToolResult {
        let p = p.0;
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
        // (item 8) + honest settle reporting (item 9). The whole
        // act + ledger sequence runs inside the session's actor; run locks
        // happen on the actor thread and never span an await.
        let quiet = p.wait_ms().unwrap_or(150);
        let selector = p.id().map(str::to_string);
        let run = self.run.clone();
        // Review P0 rigidity #4: an agent may declare how "done" means for this
        // action. A declared completion overrides the default "screen settles"
        // so silent/exit/signal actions are classified honestly (never a false
        // `settled=false`). `Resize`/`Signal` have no settle semantics and
        // always resolve the default.
        let completion = p.completion().unwrap_or(crate::capture::CompletionPolicy::StableScreen);
        self.with_sess(selector.as_deref(), move |sess| {
            // Wave G item 76: a live human control lease blocks driving.
            if let Some(refused) = lease_refused(sess) {
                return refused;
            }
            let tx = match crate::execution::execute_act_with_completion(
                sess,
                &action,
                quiet,
                quiet.saturating_add(1000),
                p.no_wait(),
                visibility,
                completion,
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
                let mut run = run.lock().unwrap();
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
                    run.record_scenario_act(
                        &sid,
                        gen,
                        serde_json::to_value(&p).unwrap_or_default(),
                    );
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
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Wait for a state condition without fixed sleeps.
    #[tool(
        name = "tui_wait",
        description = "Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges."
    )]
    pub async fn tui_wait(&self, p: Parameters<TuiWaitParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        let cond = match build_wait(&p) {
            Some(c) => c,
            None => return err(ErrorCategory::InvalidRequest, "unsupported wait condition"),
        };
        let selector = p.id.clone();
        let run = self.run.clone();
        // Go through the canonical wait executor (re-review P0), inside the
        // session actor.
        self.with_sess(selector.as_deref(), move |sess| {
            match crate::execution::execute_wait(sess, cond, p.budget_ms.unwrap_or(5000)) {
                Ok(out) => {
                    // Scoped to the resolved session generation (item 5).
                    {
                        let (sid, gen) = (sess.id.clone(), sess.generation);
                        let mut run = run.lock().unwrap();
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
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Assertions against screen/state.
    #[tool(
        name = "tui_assert",
        description = "Assert UI facts: text, text_absent, position, focus, not_clipped, dimensions, exit_code."
    )]
    pub async fn tui_assert(&self, p: Parameters<TuiAssertParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        // Wave E: `assertion: "oracle"` evaluates a declarative oracle
        // expression (shared language with contracts and audits). Any other
        // assertion name goes through the canonical assert executor.
        use crate::mcp::params::AssertAssertion as AA;
        let is_oracle = matches!(p.assertion.known(), Some(AA::Oracle));
        if is_oracle && p.text.is_none() && p.reference.is_none() {
            return err(
                ErrorCategory::InvalidRequest,
                "oracle assertion requires the expression in 'text' (e.g. text: \"modal_open()\")",
            );
        }
        let selector = p.id.clone();
        let run = self.run.clone();
        self.with_sess(selector.as_deref(), move |sess| {
            let screen = match sess.observe(40) {
                Ok(s) => s,
                Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
            };
            if is_oracle {
                let expr = p
                    .text
                    .clone()
                    .or_else(|| p.reference.clone())
                    .expect("validated above");
                let sem = semantic::analyze(&screen);
                let outcome = crate::design::eval_static(&expr, &screen, &sem);
                {
                    // Scoped to the resolved session generation (item 5).
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    let mut run = run.lock().unwrap();
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
                    let mut run = run.lock().unwrap();
                    run.record_scenario_assert(
                        &sid,
                        gen,
                        serde_json::to_value(&p).unwrap_or_default(),
                    );
                }
                if passed {
                    ok(json!({ "passed": true, "assertion": p.assertion_name() }))
                } else if let Some(ErrorCategory::InvalidRequest) = invalid {
                    // Unknown assertion name (or missing required param): a caller error,
                    // NOT a UI failure (spec section 37).
                    err(ErrorCategory::InvalidRequest, detail)
                } else {
                    err_continued(ErrorCategory::AssertionFailed, detail)
                }
            }
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Checkpoints: save / compare / list / delete (spec section 13).
    #[tool(
        name = "tui_checkpoint",
        description = "Save and compare named UI state checkpoints (durable under persistent runs)."
    )]
    pub async fn tui_checkpoint(
        &self,
        p: Parameters<TuiCheckpointParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::CheckpointAction as CA;
        let Some(ckpt_action) = p.action.known().copied() else {
            return err_invalid_selector(
                "checkpoint action",
                &p.action,
                <CA as crate::mcp::params::EnumVariants>::VARIANTS,
            );
        };
        let run = self.run.clone();
        // The actor closure owns the session side (observe) and does the
        // checkpoint work against the run under a short lock.
        self.with_sess(p.id.as_deref(), move |sess| {
            let session_id = sess.id.clone();
            let generation = sess.generation;
            match ckpt_action {
                CA::List => {
                    let run = run.lock().unwrap();
                    ok(json!({ "checkpoints": run.checkpoints.list(&session_id) }))
                }
                CA::Save => {
                    let screen = match sess.observe(40) {
                        Ok(s) => s,
                        Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                    };
                    let sem = semantic::analyze(&screen);
                    let mut run = run.lock().unwrap();
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
                CA::Compare => {
                    let name = match &p.name {
                        Some(n) => n.clone(),
                        None => {
                            return err(ErrorCategory::InvalidRequest, "compare requires 'name'")
                        }
                    };
                    let screen = match sess.observe(40) {
                        Ok(s) => s,
                        Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                    };
                    let sem = semantic::analyze(&screen);
                    let run = run.lock().unwrap();
                    match run
                        .checkpoints
                        .compare(&session_id, &name, &screen, Some(&sem))
                    {
                        Ok(json_out) => crate::mcp::helpers::ok_from_json(&json_out),
                        Err(ErrorCategory::InvalidRequest) => {
                            err(ErrorCategory::InvalidRequest, "no such checkpoint")
                        }
                        Err(c) => err(c, "checkpoint comparison failed"),
                    }
                }
                CA::Delete => {
                    let name = match &p.name {
                        Some(n) => n.clone(),
                        None => {
                            return err(ErrorCategory::InvalidRequest, "delete requires 'name'")
                        }
                    };
                    let mut run = run.lock().unwrap();
                    let removed = run.checkpoints.delete(&session_id, &name);
                    ok(json!({ "name": name, "deleted": removed }))
                }
            }
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Scenarios: record discovered workflows (spec section 13 / 4.4).
    #[tool(
        name = "tui_scenario",
        description = "Record, save, list, export, and replay interaction scenarios (session+generation scoped)."
    )]
    pub async fn tui_scenario(
        &self,
        p: Parameters<TuiScenarioParams>,
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
                let run = self.run.lock().unwrap();
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
                let run = self.run.clone();
                let started = self
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
                        match self.run.lock().unwrap().find_recording_by_name(name) {
                            Some(id) => id,
                            None => {
                                return err(
                                    ErrorCategory::InvalidRequest,
                                    format!("no recording in progress named '{}'", name),
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
                let mut run = self.run.lock().unwrap();
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
                let path: Option<String> = self
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
                let run = self.run.lock().unwrap();
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
                    let run = self.run.lock().unwrap();
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
                let run = self.run.clone();
                self.with_sess(selector.as_deref(), move |sess| {
                    // Wave G item 76: replay drives the app; a live human
                    // lease refuses the whole run before step one.
                    if let Some(refused) = lease_refused(sess) {
                        return refused;
                    }
                    let report = crate::scenario::runner::ScenarioRunner::run(&scenario, sess);
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    run.lock()
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
                })
                .await
                .unwrap_or_else(|e| e)
            }
        }
    }

    /// Recordings: cast / svg / apng / gif / mp4 (spec section 13). Microsoft
    /// provides raster capture; here we expose the asciinema `.cast` writer.
    #[tool(
        name = "tui_record",
        description = "Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures."
    )]
    pub async fn tui_record(&self, p: Parameters<TuiRecordParams>) -> rmcp::model::CallToolResult {
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
        let run = self.run.clone();
        self.with_sess(selector.as_deref(), move |sess| {
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
                ok(json!({
                    "recording": "started",
                    "boundary": "pty-bytes",
                    "note": "output is captured at the raw PTY byte boundary; call format=stop to flush to a .cast file"
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
                                    );
                                    (Some(file.to_string_lossy().to_string()), Some(r))
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
                            );
                            (None, Some(r))
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
                source_refs: Vec::new(),
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
                source_refs: Vec::new(),
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
    pub async fn tui_explore(
        &self,
        p: Parameters<TuiExploreParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::ExploreMode as EM;
        let mode_sel = p.mode.clone();
        let Some(emode) = (match &mode_sel {
            crate::mcp::params::Known::Known(m) => Some(*m),
            crate::mcp::params::Known::Other(o) => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown explore mode '{}' (expected one of: {})",
                        o,
                        <EM as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                    ),
                );
            }
        }) else {
            unreachable!()
        };
        // StateGraph needs no session at all; every other mode drives the
        // session through its actor with run locks taken on the actor thread.
        if emode == EM::StateGraph {
            let run = self.run.lock().unwrap();
            return ok(json!({
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
            }));
        }
        let selector = p.id.clone();
        let run = self.run.clone();
        let explore = p.clone();
        let server = self.clone();
        // Wave G item 76: random/semantic exploration drives the app, so the
        // human control lease blocks them. guided_candidates and state_graph
        // only read state and stay allowed — they were filtered above (the
        // StateGraph early-return) and need no session at all.
        let lease_gates_driving = matches!(emode, EM::Random | EM::Semantic);
        self.with_sess(selector.as_deref(), move |sess| {
            let p = explore;
            if lease_gates_driving {
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
            }
            match emode {
                EM::GuidedCandidates => {
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
                    let identity = crate::exploration::state_graph::StateIdentity::with_semantic(
                        &screen, &sem,
                    );
                    let current = identity.id();
                    let candidate_list = {
                        let run = run.lock().unwrap();
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
                EM::Random => {
                    let seed = p.seed.unwrap_or(4242);
                    let recording_path = p.recording_path.as_deref().map(std::path::Path::new);
                    if recording_path.is_some() {
                        sess.enable_recording(false);
                    }
                    // The budget is the authority (re-review item 13): limits come
                    // from the run's ExplorationBudget, with an optional action
                    // override; the report names the real completion reason.
                    let budget = {
                        let run = run.lock().unwrap();
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
                                let mut run = run.lock().unwrap();
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
                                let run = run.lock().unwrap();
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
                            let repro = server.minimize_crash_finding(sess, seed, &report);
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
                EM::Semantic => {
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
                        let run = run.lock().unwrap();
                        (
                            run.state_graph.budget().clone(),
                            (
                                run.state_graph.state_count(),
                                run.state_graph.transition_count(),
                            ),
                            run.contract().cloned(),
                        )
                    };
                    let max_actions = p.actions.unwrap_or(20);
                    // Local graphs during the loop (session I/O must not hold
                    // the run lock); merged into the run after.
                    let mut local_graph =
                        crate::exploration::state_graph::StateGraph::new(graph_budget.clone());
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
                        let mut run = run.lock().unwrap();
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
                EM::StateGraph => unreachable!("handled above the actor boundary"),
            }
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// UX audits: keyboard / focus / layout / resize / navigation / discoverability / states / errors / mouse / color / performance / full (spec section 14-21).
    #[tool(
        name = "tui_audit",
        description = "Run deterministic UX audits and return evidence-backed findings."
    )]
    pub async fn tui_audit(&self, p: Parameters<TuiAuditParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::AuditProfile as AP;
        let profile_sel = p
            .profile
            .clone()
            .unwrap_or(crate::mcp::params::Known::Known(AP::Full));
        let profile = match &profile_sel {
            crate::mcp::params::Known::Known(ap) => ap.engine_name().to_string(),
            crate::mcp::params::Known::Other(o) => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown audit profile '{}' (expected one of: {})",
                        o,
                        <AP as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                    ),
                );
            }
        };
        let profile_wire = match &profile_sel {
            crate::mcp::params::Known::Known(ap) => ap.as_str().to_string(),
            _ => profile.clone(),
        };
        // Wave G item 76: active profiles drive the app (keys, clicks,
        // resizes, latency sampling) and refuse under a live human lease.
        // Static/conformance-reading profiles keep observing. The engine's
        // own is_active() is the single authority for the split — the MCP
        // layer does not keep a second list.
        let profile_drives = crate::audit::orchestrator::AuditProfile::parse(&profile)
            .map(|p| p.is_active())
            .unwrap_or(false);

        // The audit ENGINE owns the static-vs-active decision (re-review
        // P0 fix 2): `full` is the composite (static + every active driver),
        // and no profile is weaker than its members. The MCP layer only
        // surfaces the resulting mode. Item 49: the loaded contract rides
        // along for profile=contract.
        let selector = p.id.clone();
        let run = self.run.clone();
        let label = p.label.clone();
        let compare_to = p.compare_to.clone();
        self.with_sess(selector.as_deref(), move |sess| {
            if profile_drives {
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
            }
            let contract = run.lock().unwrap().contract().cloned();
            let report = match crate::audit::orchestrator::run_profile_with_contract(
                sess,
                &profile,
                contract.as_ref(),
            ) {
                Ok(r) => r,
                Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
            };

            // Findings accumulate in the run context (composition root) so a
            // later audit/coverage query can see prior evidence. Driven focus
            // edges merge into the run's persistent ID-keyed FocusGraph (Wave D
            // item 36), so multiple audits accumulate traversal proof.
            // Wave G item 67: `label` stores this pass as a named baseline;
            // `compare_to` diffs the fresh pass against a stored one —
            // FIXED / NEW / PERSISTING per fingerprint, honestly reporting
            // a missing baseline instead of fabricating an empty compare.
            let mut compare_block = serde_json::Value::Null;
            let focus_summary = {
                let mut run = run.lock().unwrap();
                run.focus_graph.merge(&report.focus_graph);
                // W2.10: attach app-declared source loci where coverage
                // evidence can name the finding's control.
                run.extend_findings_with_source_refs(report.findings.clone());
                if let Some(cmp_label) = compare_to.as_deref() {
                    match run.finding_baseline(cmp_label) {
                        Some(baseline) => {
                            let compared = crate::audit::compare::compare(baseline, &report.findings);
                            compare_block = json!({
                                "baseline": cmp_label,
                                "available": true,
                                "counts": crate::audit::compare::summary(&compared),
                                "findings": compared,
                            });
                        }
                        None => {
                            let labels = run.finding_baseline_labels();
                            compare_block = json!({
                                "baseline": cmp_label,
                                "available": false,
                                "error": format!(
                                    "no finding baseline labeled '{}' (stored: {})",
                                    cmp_label,
                                    if labels.is_empty() { "none".to_string() } else { labels.join(", ") }
                                ),
                            });
                        }
                    }
                }
                if let Some(lbl) = label.as_deref() {
                    run.record_finding_baseline(lbl, report.findings.clone());
                }
                json!({
                    "nodes": run.focus_graph.nodes.len(),
                    "edges": run.focus_graph.edges.len(),
                    "tab_cycle": run.focus_graph.tab_cycle(),
                    "reverse_tab_gaps": run.focus_graph.reverse_tab_gaps(),
                })
            };
            ok(json!({
                "profile": profile_wire,
                "mode": report.mode,
                "finding_count": report.findings.len(),
                "findings": report.findings,
                "focus_graph": focus_summary,
                "labeled_as": label,
                "compare": compare_block,
            }))
        })
        .await
        .unwrap_or_else(|e| e)
    }

    /// Explain a recorded finding (Wave G review P1/P2 16): trace its evidence
    /// to the source that produced it, and condition the interpretation on the
    /// session's live terminal profile.
    #[tool(
        name = "tui_explain",
        description = "Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities the finding is conditional on."
    )]
    pub async fn tui_explain(&self, p: Parameters<TuiExplainParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        let run = self.run.clone();

        // Resolve the finding from the run ledger.
        let finding = {
            let run = run.lock().unwrap();
            run.findings()
                .iter()
                .find(|f| f.id == p.finding_id)
                .cloned()
        };
        let finding = match finding {
            Some(f) => f,
            None => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown finding id '{}' — not in the current run (run '{}'); \
                         re-run tui_audit or read tui://findings",
                        p.finding_id,
                        run.lock().unwrap().id,
                    ),
                );
            }
        };

        // Explain it, optionally conditioned on a session's live profile.
        let finding_for_job = finding.clone();
        let selector = p.id.clone();
        let explanation = match selector {
            Some(id) => self
                .with_sess(Some(&id), move |sess| {
                    let profile = sess.terminal_profile();
                    crate::terminal::explain::explain_finding(&finding_for_job, None, Some(&profile))
                })
                .await
                .unwrap_or_else(|_e| {
                    // Session lookup failed; explain without terminal context.
                    crate::terminal::explain::explain_finding(&finding, None, None)
                }),
            None => crate::terminal::explain::explain_finding(&finding, None, None),
        };

        ok(serde_json::to_value(explanation).unwrap_or(serde_json::json!({})))
    }

    /// Coverage (spec section 5; Wave F item 64). Two providers, merged:
    /// the optional tuicov executable and the NativeSemanticProtocol
    /// coverage events the run ledger accumulates.
    #[tool(
        name = "tui_coverage",
        description = "Native coverage: run ledger (native events) plus optional tuicov executable. Actions: detect, summary, collect, delta, uncovered."
    )]
    async fn tui_coverage(&self, p: Parameters<TuiCoverageParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::CoverageAction as CV;
        let action = p
            .action
            .clone()
            .unwrap_or(crate::mcp::params::Known::Known(CV::Summary));
        // The run-ledger views are answered here (session-scoped); the
        // rest delegates to the provider module.
        match action.known() {
            Some(CV::Ledger) => {
                let run = self.run.lock().unwrap();
                let entries: Vec<serde_json::Value> = run
                    .coverage_ledger
                    .iter()
                    .map(|(target, e)| {
                        json!({
                            "target": target,
                            "hits": e.hits,
                            "sessions": e.sessions,
                            "first_seen": e.first_seen,
                            "last_seen": e.last_seen,
                        })
                    })
                    .collect();
                ok(json!({
                    "entries": entries,
                    "targets": entries.len(),
                    "note": if entries.is_empty() { Some("no native coverage events yet; cooperative apps send coverage events over TUI_LAB_SEMANTIC".to_string()) } else { None },
                }))
            }
            _ => match crate::coverage::tuicov::handle(&p) {
                Ok(s) => crate::mcp::helpers::ok_from_json(&s),
                Err(e) => err(ErrorCategory::BackendError, e.to_string()),
            },
        }
    }

    /// Framework detection + native adapters (spec section 26/27).
    /// Wave F items 58–63: `adapter_snippet` returns the NativeSemanticProtocol
    /// wiring for the detected framework — the cooperation contract the app
    /// adopts in its own source.
    #[tool(
        name = "tui_framework",
        description = "Detect the TUI framework, run native probes, and fetch NativeSemanticProtocol adapter snippets (action=adapter_snippet)."
    )]
    async fn tui_framework(
        &self,
        p: Parameters<TuiFrameworkParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        let cwd = p.cwd.clone().unwrap_or_else(|| ".".into());
        // Resolve the project root through the bounded-upward ProjectLocator
        // BEFORE detection (re-review "frame detection makes a hidden
        // manifest-is-in-cwd assumption"): a monorepo caller passing a nested
        // package dir (`packages/foo/src`) should have the dependency scan run
        // against the package boundary, not the nested dir with no manifest at
        // that exact path. The locator never errors and degrades to `cwd` when
        // no manifest is found, so no-manifest raw projects detect identically.
        let project = crate::session::ProjectLocator::locate(None, &cwd);
        let detect_cwd = project.root().to_string();
        let mut det = crate::framework::detect::detect(&detect_cwd);
        // Surface the resolved root as evidence so the agent sees *which*
        // directory the detection ran against (and can pass an explicit one).
        if project.root() != cwd {
            det.evidence
                .push(format!("resolved project root: {}", project.root()));
        }
        use crate::mcp::params::FrameworkAction as FA;
        let Some(fw_action) = (match &p.action {
            crate::mcp::params::Known::Known(a) => Some(*a),
            crate::mcp::params::Known::Other(o) => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown framework action '{}' (expected one of: {})",
                        o,
                        <FA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                    ),
                );
            }
        }) else {
            unreachable!()
        };
        match fw_action {
            FA::Detect => ok(json!({ "framework": det })),
            FA::Capabilities => ok(
                json!({ "framework": det, "note": "native probes invoke project-local tooling" }),
            ),
            FA::AdapterSnippet => {
                let fw = p
                    .source
                    .clone()
                    .or_else(|| det.framework.clone())
                    .unwrap_or_else(|| "python".into());
                match crate::framework::adapters::snippet_for(&fw.to_lowercase()) {
                    Some(code) => ok(json!({
                        "framework": fw,
                        "language": if fw == "ratatui" { "rust" } else { "python" },
                        "protocol_version": crate::semantic::native::PROTOCOL_VERSION,
                        "env_var": crate::semantic::native::ENV_VAR,
                        "snippet": code,
                    })),
                    None => err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "no adapter snippet for '{}' (available: ratatui, textual, python/reference)",
                            fw
                        ),
                    ),
                }
            }
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
        description = "Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, and context (this registry as JSON)."
    )]
    pub async fn tui_run(&self, p: Parameters<TuiRunParams>) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::RunAction as RA;
        let Some(run_action) = (match &p.action {
            crate::mcp::params::Known::Known(a) => Some(*a),
            crate::mcp::params::Known::Other(o) => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown run action '{}' (expected one of: {})",
                        o,
                        <RA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                    ),
                );
            }
        }) else {
            unreachable!()
        };
        match run_action {
            RA::Status => {
                let sessions = self.sessions.list();
                ok(self.run.lock().unwrap().status(
                    sessions
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ))
            }
            RA::Persist => {
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
            RA::Close => {
                // Drain terminal-event queues into the run (Wave B item 14)
                // before the flush so the event logs land in the artifacts.
                // Wave G item 73: each drain is one actor call — no guard is
                // held across the loop, so there is no re-entrancy dance and
                // no close-path deadlock to avoid.
                for sid in self.sessions.list() {
                    if let Ok(events) = self.sessions.drain_events(&sid).await {
                        self.run.lock().unwrap().hold_events(&sid, events);
                    }
                }
                let already;
                let summary;
                let kill;
                let result;
                {
                    let mut run = self.run.lock().unwrap();
                    already = run.is_closed();
                    let sessions = self.sessions.list();
                    summary = run.status(
                        sessions
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    );
                    kill = p.kill_sessions.unwrap_or(false);
                    result = run.close();
                }
                if let Err(e) = result {
                    return err(
                        ErrorCategory::InternalError,
                        format!("close flush failed: {e}"),
                    );
                }
                // Sessions survive close unless explicitly requested.
                let stopped: Vec<String> = if kill {
                    self.sessions.stop_all().await
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
            // Wave G item 68: the capability registry as JSON — the
            // machine-readable answer to "what can this server do".
            RA::Context => ok(json!({
                "registry": crate::mcp::registry::to_json(),
            })),
            // Wave G item 75: enumerate persisted runs under a resolved
            // root (explicit root, else the primary session's cwd; neither
            // present is invalid_request — the same no-guessing rule persist
            // follows). Corrupt entries are named in place, never dropped.
            RA::List => {
                let base: String = match p.root.clone() {
                    Some(r) => r,
                    None => match self.run.lock().unwrap().primary_session_cwd() {
                        Some(cwd) => cwd.to_string(),
                        None => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                "no root to scan for runs: pass 'root', or start a session whose LaunchSpec.cwd is set",
                            )
                        }
                    },
                };
                match crate::run::RunContext::list_persisted(std::path::Path::new(&base)) {
                    Ok(mut entries) => {
                        // A directory with no run.json lands here with its
                        // `skipped` reason; keep it — corruption is evidence.
                        let runs: Vec<_> = entries
                            .drain(..)
                            .filter(|e| e.get("run_id").is_some())
                            .collect();
                        let skipped: Vec<_> = entries
                            .into_iter()
                            .filter(|e| e.get("skipped").is_some())
                            .collect();
                        ok(json!({
                            "base": base,
                            "runs": runs,
                            "skipped": skipped,
                            "current_run": self.run.lock().unwrap().id.clone(),
                        }))
                    }
                    Err(e) => err(ErrorCategory::InvalidRequest, e.to_string()),
                }
            }
            // Wave G item 74: restore a persisted run as the live run. The
            // live run is flushed first (identity-preserving; an ephemeral
            // run with nothing durable simply ends), sessions are left
            // untouched (they belonged to the old run; the restored run
            // starts with none — the manifest's launch specs are on disk
            // for re-creation), and the restored run continues the same
            // id, ledger, findings, graphs, scenarios, and checkpoints.
            RA::Resume => {
                // Resolve the target directory: explicit run_dir wins, else
                // run_id under the resolved base.
                let run_dir: std::path::PathBuf = if let Some(d) = p.run_dir.clone() {
                    std::path::PathBuf::from(d)
                } else if let Some(rid) = p.run_id.clone() {
                    let base: String = match p.root.clone() {
                        Some(r) => r,
                        None => match self.run.lock().unwrap().primary_session_cwd() {
                            Some(cwd) => cwd.to_string(),
                            None => {
                                return err(
                                    ErrorCategory::InvalidRequest,
                                    "resume needs 'run_dir', or 'run_id' with a resolvable 'root' (explicit, or a session cwd)",
                                )
                            }
                        },
                    };
                    let candidate =
                        crate::run::RunContext::resolve_run_dir(std::path::Path::new(&base), &rid);
                    match candidate {
                        Some(d) => d,
                        None => {
                            return err(
                                ErrorCategory::InvalidRequest,
                                format!("no persisted run '{}' under {}", rid, base),
                            )
                        }
                    }
                } else {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "resume requires 'run_id' (from tui_run action=list) or 'run_dir'",
                    );
                };
                let restored = match crate::run::RunContext::restore(&run_dir) {
                    Ok(r) => r,
                    Err(e) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("cannot restore {}: {e}", run_dir.to_string_lossy()),
                        )
                    }
                };
                // Swap under a short lock (never across an await). The old
                // run is dropped after its Drop-less flush attempt; sessions
                // stay live regardless — resume never kills anything.
                let manifest_like = {
                    let mut guard = self.run.lock().unwrap();
                    let prev_id = guard.id.clone();
                    let restored_id = restored.id.clone();
                    let restored_dir = restored
                        .run_dir()
                        .map(|d| d.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let summary = restored.status(Vec::new());
                    *guard = restored;
                    json!({
                        "resumed": true,
                        "run_id": restored_id,
                        "previous_run": prev_id,
                        "artifact_root": restored_dir,
                        "status": summary,
                        "note": "sessions are not restored; re-create them with tui_session start and the run will correlate them. history_complete=false runs replay only from their declared first_available_seq",
                    })
                };
                ok(manifest_like)
            }
            // ── repair: RepairPackets for every finding (audit item) ──
            RA::Repair => {
                let (packets, skipped) = {
                    let run = self.run.lock().unwrap();
                    run.repair_packets()
                };
                if packets.is_empty() && skipped == 0 {
                    return ok(json!({
                        "packets": [],
                        "skipped": 0,
                        "note": "no findings recorded in this run — run an audit first (tui_audit action=run)",
                    }));
                }
                ok(json!({
                    "packets": packets,
                    "skipped": skipped,
                    "note": if skipped > 0 {
                        Some(format!("{skipped} finding(s) could not form a packet (no evidence) and were skipped"))
                    } else {
                        None
                    },
                }))
            }
        }
    }

    /// Contracts (Wave E items 39–49): load a design contract, validate the
    /// document, check the running app's conformance (PASS/FAIL/WARN), and
    /// compare conformance across points in time. Loading a contract also
    /// installs its `volatile_patterns` into the session's normalization
    /// policy (item 48) and arms contract-guided exploration (item 49).
    #[tool(
        name = "tui_contract",
        description = "Design contracts: load, validate, conformance status, and baseline compare (regressions become findings)."
    )]
    pub async fn tui_contract(
        &self,
        p: Parameters<TuiContractParams>,
    ) -> rmcp::model::CallToolResult {
        let p = p.0;
        use crate::mcp::params::ContractAction as CT;
        let Some(ct_action) = (match &p.action {
            crate::mcp::params::Known::Known(a) => Some(*a),
            crate::mcp::params::Known::Other(o) => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown contract action '{}' (expected one of: {})",
                        o,
                        <CT as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                    ),
                );
            }
        }) else {
            unreachable!()
        };
        match ct_action {
            // ── load: parse + validate + remember + apply policy ──
            CT::Load => {
                let Some(path) = p.path.clone() else {
                    return err(ErrorCategory::InvalidRequest, "load requires 'path'");
                };
                let contract =
                    match crate::design::load_design_contract(std::path::Path::new(&path)) {
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
                // Apply to EVERY live session (item 48 semantics): one actor
                // job per session, none blocking another.
                let mut applied: Vec<String> = Vec::new();
                for sid in self.sessions.list() {
                    let policy_for_session = policy.clone();
                    if let Ok(id) = self
                        .with_sess(Some(&sid), move |sess| {
                            sess.set_normalization_policy(policy_for_session.clone());
                            sess.id.clone()
                        })
                        .await
                    {
                        applied.push(id);
                    }
                }
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
            CT::Validate => {
                let Some(path) = p.path.clone() else {
                    return err(ErrorCategory::InvalidRequest, "validate requires 'path'");
                };
                // Parse WITHOUT the load-time hard stop, so a report of every
                // problem comes back instead of only the first.
                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        return err(ErrorCategory::InvalidRequest, format!("cannot read: {e}"))
                    }
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
                        let verdict = results
                            .iter()
                            .fold(crate::design::Verdict::Pass, |acc, r| acc.merge(r.verdict));
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
            CT::Status => {
                let contract =
                    {
                        let run = self.run.lock().unwrap();
                        match run.contract() {
                            Some(c) => c.clone(),
                            None => return err(
                                ErrorCategory::InvalidRequest,
                                "no contract loaded; call tui_contract action=load path=... first",
                            ),
                        }
                    };
                self.check_contract_against(p.id.as_deref(), contract).await
            }
            // ── compare: run conformance now, diff against the baseline ──
            CT::Compare => {
                let contract =
                    {
                        let run = self.run.lock().unwrap();
                        match run.contract() {
                            Some(c) => c.clone(),
                            None => return err(
                                ErrorCategory::InvalidRequest,
                                "no contract loaded; call tui_contract action=load path=... first",
                            ),
                        }
                    };
                let current = match self
                    .check_contract_inner(p.id.as_deref(), contract.clone())
                    .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return err(ErrorCategory::BackendError, e.to_string()),
                    Err(e) => return e,
                };
                let label = p.label.clone().unwrap_or_else(|| "current".into());
                let mut run = self.run.lock().unwrap();
                let baseline_label = p.baseline.clone().unwrap_or_else(|| "baseline".into());
                let baseline = run.contract_baselines().get(&baseline_label).cloned();
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
                                    source_refs: Vec::new(),
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
        }
    }

    /// Actor-backed conformance check with run-ledger recording.
    async fn check_contract_against(
        &self,
        id: Option<&str>,
        contract: crate::design::ProjectContract,
    ) -> rmcp::model::CallToolResult {
        let selector = id.map(str::to_string);
        let run = self.run.clone();
        match self
            .with_sess(selector.as_deref(), move |sess| {
                // Conformance drives the app (declared keys, resizes,
                // Escape/Tab probes); the human control lease (item 76)
                // refuses it like any other driving path. The closure's
                // Result discriminates lease-refusal (left) from the
                // engine result (right).
                if let Some(refused) = lease_refused(sess) {
                    return Err(refused);
                }
                Ok(crate::design::check_contract(sess, &contract))
            })
            .await
        {
            Ok(Ok(Ok(report))) => {
                // Findings feed the run ledger (item 49).
                let findings = report.findings();
                let summary = report.summary();
                let results = report.results.clone();
                let mut run = run.lock().unwrap();
                run.extend_findings_with_source_refs(findings);
                run.record_contract_baseline("baseline", &report);
                ok(json!({
                    "verdict": report.verdict.as_str(),
                    "summary": summary,
                    "results": results,
                }))
            }
            Ok(Ok(Err(e))) => err(ErrorCategory::BackendError, e.to_string()),
            Ok(Err(refused)) => refused,
            Err(e) => e,
        }
    }

    async fn check_contract_inner(
        &self,
        id: Option<&str>,
        contract: crate::design::ProjectContract,
    ) -> Result<anyhow::Result<crate::design::ContractReport>, rmcp::model::CallToolResult> {
        let selector = id.map(str::to_string);
        match self
            .with_sess(selector.as_deref(), move |sess| {
                // Same lease rule as check_contract_against (item 76).
                if let Some(refused) = lease_refused(sess) {
                    return Err(refused);
                }
                Ok(crate::design::check_contract(sess, &contract))
            })
            .await
        {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(refused)) => Err(refused),
            Err(e) => Err(e),
        }
    }
}

/// Diff two contract reports result-by-result, keyed by `group + name`.
/// Regressions: Pass→Fail (and Pass→Warn for required checks). Fixed:
/// Fail→Pass, Warn→Pass. Verdict-neutral changes (Warn→Fail on optional
/// checks etc.) are reported as regressions too — stricter is a regression
/// whenever the check was required.
/// One (key, before, after) row per changed check in a contract comparison.
type ContractCheckDiff = Vec<(
    String,
    crate::design::CheckResult,
    crate::design::CheckResult,
)>;

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
            (Verdict::Fail, Verdict::Pass)
                | (Verdict::Warn, Verdict::Pass)
                | (Verdict::Fail, Verdict::Warn)
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
// The resource methods below live on the same impl — the macro skips any
// method that is already hand-written (has_method detection), so resources
// need no second type or wrapper trait.
#[rmcp::tool_handler]
impl ServerHandler for TuiLabServer {
    // ── MCP resources (Wave G item 72) ─────────────────────────────────
    // The live surface the agent can subscribe to instead of polling:
    // current run manifest, per-session semantic/screen snapshots, and the
    // findings ledger. Everything is read-through (no caching): a read
    // reflects the session state at read time, and unknown ids are honest
    // `resource_not_found` errors, never empty placeholders.

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::model::ErrorData> {
        use rmcp::model::{ListResourcesResult, Resource};
        let run_id = self.run.lock().unwrap().id.clone();
        let items = vec![
            Resource::new("tui://findings", "findings").with_description(
                "Findings accumulated this run (audits, contracts, exploration).",
            ),
            Resource::new(format!("tui://runs/{run_id}"), format!("run-{run_id}"))
                .with_description("This run's status and manifest."),
        ];
        Ok(ListResourcesResult::with_all_items(items))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, rmcp::model::ErrorData> {
        use rmcp::model::{ListResourceTemplatesResult, ResourceTemplate};
        let templates = vec![
            ResourceTemplate::new("tui://runs/{run_id}", "run")
                .with_description("Run status + manifest for the named run id."),
            ResourceTemplate::new("tui://sessions/{session_id}/semantic", "session-semantic")
                .with_description("Live semantic screen: regions, controls, focus, affordances."),
            ResourceTemplate::new("tui://sessions/{session_id}/screen", "session-screen")
                .with_description("Live screen text + geometry."),
        ];
        Ok(ListResourceTemplatesResult::with_all_items(templates))
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::model::ErrorData> {
        use rmcp::model::{ReadResourceResult, ResourceContents};
        let uri = request.uri.clone();
        let contents = self.resolve_resource(&uri).await?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(contents, uri)]).into())
    }
}
