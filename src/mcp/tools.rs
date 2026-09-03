//! MCP surface: the 16 tools the agent sees (spec section 13 + later waves).
//! Internally there are hundreds of operations; the agent sees `tui_session`,
//! `tui_observe`, `tui_act`, `tui_wait`, `tui_assert`, `tui_checkpoint`,
//! `tui_scenario`, `tui_record`, `tui_explore`, `tui_audit`, `tui_explain`,
//! `tui_coverage`, `tui_framework`, `tui_run`, `tui_contract` — and the
//! registry (`tui_run action=context`) is the single authoritative list.

use std::sync::Arc;

use rmcp::serde_json::json;
use rmcp::tool;
use rmcp::tool_router;
use rmcp::ServerHandler;

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use crate::mcp::params::*;
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
    /// Which run each live session belongs to (review P0.2): session id →
    /// the run id that launched it. A session is only usable while it
    /// belongs to the CURRENT run; resuming a different run must not let a
    /// stale session drive it and spill foreign evidence in (run
    /// provenance). Populated at launch/attach, dropped when the session
    /// stops.
    session_owners: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
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
            session_owners: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// A server over a caller-supplied run context (integration tests seed
    /// ledger/graph evidence directly, mirroring how `tui_run action=resume`
    /// swaps in a restored run). Not a public API surface — hidden.
    #[doc(hidden)]
    pub fn with_run(run: crate::run::RunContext) -> Self {
        TuiLabServer {
            sessions: Arc::new(SessionPool::new()),
            run: Arc::new(std::sync::Mutex::new(run)),
            session_owners: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
                        return Some(serde_json::to_string_pretty(&profile).unwrap_or_default());
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
    ///
    /// Review P0.1: a closed run must not accept new traffic. Every
    /// session-driving tool funnels through here, so the guard lives once
    /// at the choke point rather than in each handler — `tui_act`,
    /// `tui_wait`, `tui_probe`, `tui_assert` et al. refuse while the
    /// current run is closed, and the agent is told to resume or start a
    /// fresh run. Read-only run surfaces (`tui_run status/list`, explain,
    /// replay) do not go through this path and stay available.
    async fn with_sess<R, F>(
        &self,
        id: Option<&str>,
        job: F,
    ) -> Result<R, rmcp::model::CallToolResult>
    where
        R: Send + 'static,
        F: FnOnce(&mut crate::session::Session) -> R + Send + 'static,
    {
        if self.run.lock().unwrap().is_closed() {
            let run_id = self.run.lock().unwrap().id.clone();
            return Err(err(
                ErrorCategory::RunClosed,
                format!(
                    "current run {run_id} is closed; resume it (tui_run action=resume) or start a fresh run before driving sessions"
                ),
            ));
        }
        // Review P0.2: a session resolves only while it belongs to the
        // CURRENT run. A session launched under run A must never be driven
        // after the current run became B (resume/`new`) — its traffic would
        // spill foreign evidence into B. The owning run is bound at
        // launch/attach; resolution against a non-owner fails loudly instead
        // of silently re-targeting the process.
        let cur_run = self.run.lock().unwrap().id.clone();
        let target_id = match id {
            Some(i) => Some(i.to_string()),
            None => self.sessions.active_id(),
        };
        if let Some(sid) = target_id {
            let bound = self
                .session_owners
                .lock()
                .ok()
                .and_then(|m| m.get(&sid).cloned());
            if let Some(own) = bound {
                if own != cur_run {
                    return Err(err(
                        ErrorCategory::NoSession,
                        format!(
                            "session '{}' is bound to run {own}, not the current run {cur_run}; stop it and re-launch under the current run, or resume run {own}",
                            sid
                        ),
                    ));
                }
            }
        }
        self.sessions
            .with_session(id, job)
            .await
            .map_err(|e| err(e.category(), e.to_string()))
    }

    /// Bind a freshly launched session to the current run, and its owner on
    /// the same short lock. Called right after start/attach succeeds.
    fn bind_session_owner(&self, id: &str) {
        let run_id = self.run.lock().unwrap().id.clone();
        self.session_owners
            .lock()
            .expect("session owner lock")
            .insert(id.to_string(), run_id);
    }
}

impl Default for TuiLabServer {
    fn default() -> Self {
        TuiLabServer::new()
    }
}

pub(crate) mod handlers;

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl TuiLabServer {
    /// Session lifecycle: start / restart / stop / list / status.
    #[tool(
        name = "tui_session",
        description = "Manage TUI sessions: start, restart, stop, list, status, attach to an existing process (brownfield), plus the human control lease (lease/release)."
    )]
    pub async fn tui_session(
        &self,
        p: Parameters<TuiSessionParams>,
    ) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::session::tui_session(self, p).await
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
        crate::mcp::tools::handlers::observe::tui_observe(self, p).await
    }

    /// Drive input: key / keys / type / paste / mouse_* / resize / signal / raw.
    /// After acting, returns a BEFORE/AFTER transition (spec section 11/30).
    #[tool(
        name = "tui_act",
        description = "Drive input. Returns a screen transition after the action. Actions: key, keys, type, paste, raw, mouse_click, mouse_press, mouse_release, mouse_move, mouse_drag, mouse_scroll, resize, signal."
    )]
    pub async fn tui_act(&self, p: Parameters<TuiActRequest>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::interact::tui_act(self, p).await
    }

    /// The troubleshooting primitive (re-review Wave-2): run one small
    /// experiment and report EVERYTHING materially different — causal
    /// events, the settled frame, the transition, watched material changes.
    #[tool(
        name = "tui_probe",
        description = "Run one small experiment against the terminal and get everything materially different: baseline vs settled after-frame, causal terminal events inside the probe window, screen/semantic transition, watched material changes (cursor/focus/controls/regions/style/process). stimulus {kind:none} = drift probe."
    )]
    pub async fn tui_probe(&self, p: Parameters<TuiProbeParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::probe::tui_probe(self, p).await
    }

    /// Wait for a state condition without fixed sleeps.
    #[tool(
        name = "tui_wait",
        description = "Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges."
    )]
    pub async fn tui_wait(&self, p: Parameters<TuiWaitParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::observe::tui_wait(self, p).await
    }

    /// Assertions against screen/state.
    #[tool(
        name = "tui_assert",
        description = "Assert UI facts: text, text_absent, position, focus, not_clipped, dimensions, exit_code."
    )]
    pub async fn tui_assert(&self, p: Parameters<TuiAssertParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::observe::tui_assert(self, p).await
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
        crate::mcp::tools::handlers::interact::tui_checkpoint(self, p).await
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
        crate::mcp::tools::handlers::scenario::tui_scenario(self, p).await
    }

    /// Recordings: cast / svg / apng / gif / mp4 (spec section 13). Microsoft
    /// provides raster capture; here we expose the asciinema `.cast` writer.
    #[tool(
        name = "tui_record",
        description = "Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures."
    )]
    pub async fn tui_record(&self, p: Parameters<TuiRecordParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::scenario::tui_record(self, p).await
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
            let _ = run.extend_findings(vec![crate::audit::Finding {
                id: format!("EXPLORE-CRASH-{}", exit.action_index),
                rule_id: None,
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
            let _ = run.extend_findings(vec![crate::audit::Finding {
                id: format!("EXPLORE-CRASH-{}", exit.action_index),
                rule_id: None,
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
        crate::mcp::tools::handlers::explore::tui_explore(self, p).await
    }

    /// UX audits: keyboard / focus / layout / resize / navigation / discoverability / states / errors / mouse / color / performance / full (spec section 14-21).
    #[tool(
        name = "tui_audit",
        description = "Run deterministic UX audits and return evidence-backed findings."
    )]
    pub async fn tui_audit(&self, p: Parameters<TuiAuditParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::audit::tui_audit(self, p).await
    }

    /// Explain a recorded finding (Wave G review P1/P2 16): trace its evidence
    /// to the source that produced it, and condition the interpretation on the
    /// session's live terminal profile.
    #[tool(
        name = "tui_explain",
        description = "Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities the finding is conditional on."
    )]
    pub async fn tui_explain(
        &self,
        p: Parameters<TuiExplainParams>,
    ) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::audit::tui_explain(self, p).await
    }

    /// Coverage (spec section 5; Wave F item 64). Two providers, merged:
    /// the optional tuicov executable and the NativeSemanticProtocol
    /// coverage events the run ledger accumulates.
    #[tool(
        name = "tui_coverage",
        description = "Native coverage: run ledger (native events) plus optional tuicov executable. Actions: detect, summary, collect, delta (since_seq cursor), uncovered, ledger."
    )]
    pub async fn tui_coverage(
        &self,
        p: Parameters<TuiCoverageParams>,
    ) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::coverage::tui_coverage(self, p).await
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
        crate::mcp::tools::handlers::framework::tui_framework(self, p).await
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
        description = "Run lifecycle: status, persist (ephemeral→durable, same identity), close, new (fresh ephemeral run), list persisted runs, resume one as the live run, diagnose/repair (diagnostic evidence context per finding — provenance-tiered loci, verification plan, next observations; never edits), bundle (one finding + before/after regression diff), and context (this registry as JSON)."
    )]
    pub async fn tui_run(&self, p: Parameters<TuiRunParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::run::tui_run(self, p).await
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
        crate::mcp::tools::handlers::contract::tui_contract(self, p).await
    }

    /// Actor-backed conformance check with run-ledger recording.
    async fn check_contract_against(
        &self,
        id: Option<&str>,
        contract: crate::design::ProjectContract,
        mode_override: Option<crate::design::ContractMode>,
    ) -> rmcp::model::CallToolResult {
        let selector = id.map(str::to_string);
        let run = self.run.clone();
        let mode = mode_override;
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
                Ok(crate::design::check_contract_with_mode(
                    sess, &contract, mode,
                ))
            })
            .await
        {
            Ok(Ok(Ok(report))) => {
                // Findings feed the run ledger (item 49). Baselines are
                // recorded ONLY by the explicit `action=baseline` (re-review
                // item 31): a silent auto-record under "baseline" made every
                // status check clobber the comparison point.
                let findings = report.findings();
                let summary = report.summary();
                let results = report.results.clone();
                let mut run = run.lock().unwrap();
                let _ = run.extend_findings_with_source_refs(findings);
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
        mode_override: Option<crate::design::ContractMode>,
    ) -> Result<anyhow::Result<crate::design::ContractReport>, rmcp::model::CallToolResult> {
        let selector = id.map(str::to_string);
        let mode = mode_override;
        match self
            .with_sess(selector.as_deref(), move |sess| {
                // Same lease rule as check_contract_against (item 76).
                if let Some(refused) = lease_refused(sess) {
                    return Err(refused);
                }
                Ok(crate::design::check_contract_with_mode(
                    sess, &contract, mode,
                ))
            })
            .await
        {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(refused)) => Err(refused),
            Err(e) => Err(e),
        }
    }
}

/// Parse the `mode` tool parameter; `None` means "use the contract's own
/// mode". Unknown strings are `None` here because the typed enum surfaces
/// them as invalid_request at the parameter layer.
/// Item 32: resolve the typed mode selector into the engine's mode
/// override. `None` (absent) means "use the contract document's mode";
/// `Known::Other` is the caller's typo and names the accepted set.
fn contract_mode_override(
    mode: &Option<crate::mcp::params::Known<crate::mcp::params::ContractModeParam>>,
) -> Result<Option<crate::design::ContractMode>, String> {
    use crate::mcp::params::{ContractModeParam as CMP, Known};
    match mode {
        None => Ok(None),
        Some(Known::Known(CMP::Advisory)) => Ok(Some(crate::design::ContractMode::Advisory)),
        Some(Known::Known(CMP::Validation)) => Ok(Some(crate::design::ContractMode::Validation)),
        Some(Known::Known(CMP::Strict)) => Ok(Some(crate::design::ContractMode::Strict)),
        Some(Known::Other(s)) => Err(format!(
            "unknown mode '{s}': expected one of advisory, validation, strict"
        )),
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
            // Item 32: Unverified/Unsupported are not failures — moving
            // into them is a loss of evidence, surfaced separately, never
            // counted as a regression (which would punish the harness for
            // its own blind spots).
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
        // One declaration (review §12): every RESOURCES row whose URI has a
        // {placeholder} becomes a template here. A new resource is added to
        // the registry table once; discovery follows with no second list to
        // drift (the old hand-written trio had already dropped
        // terminal-profile while the resolver served it).
        let templates = crate::mcp::registry::RESOURCES
            .iter()
            .filter(|r| r.uri.contains('{'))
            .map(|r| {
                let name = r
                    .uri
                    .trim_start_matches("tui://")
                    .replace("/{", "/")
                    .replace('}', "");
                ResourceTemplate::new(r.uri, name).with_description(r.description)
            })
            .collect();
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

/// Read the declared run id out of a persisted run directory (review P0.2).
/// Used to decide which live sessions legitimately belong to the run being
/// resumed. The manifest is authoritative; a directory without one cannot
/// be a valid resume target.
fn running_id(run_dir: &std::path::Path) -> String {
    crate::run::manifest::load(run_dir)
        .map(|m| m.run_id)
        .unwrap_or_default()
}
