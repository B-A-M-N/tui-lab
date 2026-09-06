//! MCP surface: the tools the agent sees (spec section 13 + later waves).
//! The authoritative count lives in [`crate::mcp::registry::TOOLS`] — never
//! a hand-maintained number here (audit P1-49).
//! Internally there are hundreds of operations; the agent sees `tui_session`,
//! `tui_observe`, `tui_act`, `tui_wait`, `tui_assert`, `tui_checkpoint`,
//! `tui_scenario`, `tui_record`, `tui_explore`, `tui_audit`, `tui_explain`,
//! `tui_coverage`, `tui_framework`, `tui_run`, `tui_contract` — and the
//! registry (`tui_run action=context`) is the single authoritative list.

use std::sync::Arc;


use rmcp::tool;
use rmcp::tool_router;
use rmcp::ServerHandler;


use crate::error::ErrorCategory;
use crate::mcp::helpers::err;
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
    pub(crate) sessions: Arc<SessionPool>,
    pub(crate) run: Arc<std::sync::Mutex<crate::run::RunContext>>,
    /// Which run each live session belongs to (review P0.2) and the
    /// previewed intent plans (finding 3D) — both live in
    /// [`crate::mcp::ownership`]; this struct keeps the Arc handles so
    /// handler child modules can clone-share them across the actor await.
    session_owners: Arc<std::sync::Mutex<crate::mcp::ownership::SessionOwnership>>,
    intent_plans: Arc<std::sync::Mutex<crate::mcp::ownership::IntentPlanStore>>,
}

impl TuiLabServer {
    pub fn new() -> Self {
        TuiLabServer {
            sessions: Arc::new(SessionPool::new()),
            run: Arc::new(std::sync::Mutex::new(crate::run::RunContext::ephemeral())),
            session_owners: Arc::new(std::sync::Mutex::new(
                crate::mcp::ownership::SessionOwnership::new(),
            )),
            intent_plans: Arc::new(std::sync::Mutex::new(
                crate::mcp::ownership::IntentPlanStore::new(),
            )),
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
            session_owners: Arc::new(std::sync::Mutex::new(
                crate::mcp::ownership::SessionOwnership::new(),
            )),
            intent_plans: Arc::new(std::sync::Mutex::new(
                crate::mcp::ownership::IntentPlanStore::new(),
            )),
        }
    }

    /// Test/lifecycle escape: the pool handle. Integration tests use it
    /// for cleanup kills that deliberately bypass lifecycle authorization
    /// (mirrors `with_run`'s doc-hidden test surface; not a public API).
    #[doc(hidden)]
    pub fn pool_handle(&self) -> std::sync::Arc<SessionPool> {
        self.sessions.clone()
    }

    /// Finding 3D: the shared intent-plan store handle for the handler
    /// family (child modules cannot capture `&self` across the actor
    /// await, so they take the Arc).
    pub(crate) fn plans_handle(
        &self,
    ) -> Arc<std::sync::Mutex<crate::mcp::ownership::IntentPlanStore>> {
        self.intent_plans.clone()
    }

    /// Resolve a `tui://` resource URI (Wave G item 72). The whole
    /// resolver lives in [`crate::mcp::resources`]; this is the façade
    /// hook the rmcp trait calls.
    async fn resolve_resource(&self, uri: &str) -> Result<String, rmcp::model::ErrorData> {
        crate::mcp::resources::resolve::resource(self, uri).await
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
            let run_id = self.run.lock().unwrap().id().to_string();
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
        let cur_run = self.run.lock().unwrap().id().to_string();
        let target_id = match id {
            Some(i) => Some(i.to_string()),
            None => self.sessions.active_id(),
        };
        if let Some(sid) = target_id {
            let bound = self
                .session_owners
                .lock()
                .ok()
                .and_then(|m| m.owner_of(&sid));
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
            .map_err(|e| match e.details() {
                // Finding 12: a busy refusal carries the retry window
                // structurally — the agent reads `retry_after_ms` instead of
                // regex-matching the prose.
                Some(details) => {
                    crate::mcp::helpers::err_with_details(e.category(), e.to_string(), details)
                }
                None => err(e.category(), e.to_string()),
            })
    }

    /// Like `with_sess`, but the closure ALSO receives the run ticket
    /// captured under the SAME authorization locks (beta-audit P0-6): the
    /// closed-run guard, the ownership guard, and the ticket's run-id read
    /// are one atomic observation. Driving handlers commit evidence
    /// through ticket-verified paths, so an authorization-to-commit run
    /// switch drops the evidence instead of spilling it into the new run.
    /// Jobs that never write run evidence can keep using plain `with_sess`.
    async fn with_sess_authorized<R, F>(
        &self,
        id: Option<&str>,
        job: F,
    ) -> Result<R, rmcp::model::CallToolResult>
    where
        R: Send + 'static,
        F: FnOnce(
            &mut crate::session::Session,
            crate::execution::RunTicket,
        ) -> R
            + Send
            + 'static,
    {
        // Capture the ticket INSIDE the same lock windows the guards use:
        // the last lock read here is the ownership check, so re-reading the
        // run id immediately after (still before any await on the actor)
        // observes the same run unless a switch lands in that sliver — and
        // the commit-time verify catches even that: the ticket names the
        // run this job was authorized under AS OF the authorization
        // locks, and any later commit into a different id refuses.
        let sink = crate::execution::RunEvidenceSink::capture(&self.run);
        let ticket = sink.ticket().clone();
        // Beta-audit P0-7: the sink rides the SESSION for the job's whole
        // actor turn, so every transaction the canonical executor produces
        // inside it — including from paths that never see the run Arc
        // (audit drivers, exploration, conformance, repro) — commits
        // through the ONE ticket-verified pipeline.
        self.with_sess(id, move |sess| {
            sess.install_evidence_sink(sink);
            let out = job(sess, ticket);
            sess.take_evidence_sink();
            out
        })
        .await
    }

    /// Bind a freshly launched session to the current run, and its owner on
    /// the same short lock. Called right after start/attach succeeds.
    fn bind_session_owner(&self, id: &str) {
        let run_id = self.run.lock().unwrap().id().to_string();
        self.session_owners
            .lock()
            .expect("session owner lock")
            .bind(id, &run_id);
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

    /// Semantic intents (review P1): resolve a target + verb into a
    /// focus-secured plan; execute=true runs it. The agent sees the exact
    /// steps and risk BEFORE anything is sent.
    #[tool(
        name = "tui_intent",
        description = "Act by semantic intent: resolve a target ({by:id|text|role|focused}) + verb (activate/focus/click/toggle/select/open/type) into a focus-secured execution plan and report its exact steps and risk. Plan-only by default — pass execute=true to run the steps in order (focus click, focus guard, payload action) under the same lease rules as tui_act."
    )]
    pub async fn tui_intent(&self, p: Parameters<TuiIntentParams>) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::interact::tui_intent(self, p).await
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

    /// Assertions against screen/state. Audit P1-51: the description is
    /// generated from the authoritative assertion enum (same expression the
    /// registry uses) — a new assertion variant updates the wire description
    /// without anyone remembering this string.
    #[tool(
        name = "tui_assert",
        description = "Assert UI facts; unknown assertions are invalid_request (caller error), never assertion_failed (UI failure). Assertions: text, text_absent, position, focus, focused_not, not_clipped, dimensions, exit_code, region, snapshot, structure, control_exists, oracle (the shared contract oracle language)."
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

    /// Exploration: random / guided_candidates / coverage_guided / replay (spec section 4/25).
    #[tool(
        name = "tui_explore",
        description = "Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, or the exploration state graph (modes: random, guided_candidates, semantic, state_graph). Driving modes are blocked while a human lease is live. Replay of discovered flows is tui_scenario's job — exploration returns evidence, not another reasoning loop."
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

    /// Lifecycle authorization for session-mutating operations (stop/
    /// restart/lease-release) — deliberately NOT the driving gate
    /// (with_sess). Beta-audit P0-4: those lifecycle handlers previously
    /// piggybacked on with_sess to read the lease, but with_sess refuses
    /// BEFORE the lease is read when the run is closed or the session is
    /// foreign — and the handlers discarded that Err and stopped the
    /// process anyway. A leased foreign (or leased-under-closed-run)
    /// session could be terminated, violating both the lease guarantee
    /// and run provenance.
    ///
    /// Check order is the safety order: (1) the LIVE LEASE on the actor
    /// itself — resolved regardless of run-open state; (2) ownership —
    /// a session bound to another run is not this run's to mutate.
    /// `Ok(())` means the caller may proceed with the mutation.
    async fn authorize_lifecycle(
        &self,
        id: &str,
        operation: &str,
    ) -> Result<(), rmcp::model::CallToolResult> {
        // 1. The lease lives on the session actor: resolve it directly
        // through the pool, never through with_sess (whose run guards run
        // first and would mask the lease).
        if !self.sessions.list().iter().any(|s| s == id) {
            // A session that no longer resolves has nothing to authorize;
            // the caller's mutation is idempotent ("already gone" is
            // success at the pool, stop's own contract).
            return Ok(());
        }
        let lease = self
            .sessions
            .with_session(Some(id), |sess| sess.driving_blocked())
            .await;
        match lease {
            Ok(Some(lease)) => {
                return Err(crate::mcp::helpers::err_with_details(
                    ErrorCategory::ControlLeased,
                    format!(
                        "session '{id}' is leased to '{}' ({}ms remaining); {operation} would kill the process they are driving",
                        lease.holder,
                        lease.remaining_ms()
                    ),
                    serde_json::json!({
                        "session": id,
                        "holder": lease.holder,
                        "retry_after_ms": lease.remaining_ms(),
                    }),
                ));
            }
            // Lease expired or absent: fall through to the ownership check.
            _ => {}
        }
        // 2. Ownership: a session bound to a DIFFERENT run is not this
        // run's to stop/restart — its lifecycle belongs to that run.
        // (Unbound sessions carry no objection here, matching the
        // driving gate's adoption rule.)
        let cur_run = self.run.lock().unwrap().id().to_string();
        let owner = self
            .session_owners
            .lock()
            .ok()
            .and_then(|m| m.owner_of(id));
        if let Some(own) = owner {
            if own != cur_run {
                return Err(err(
                    ErrorCategory::NoSession,
                    format!(
                        "session '{id}' is bound to run {own}, not the current run {cur_run}; {operation} is refused — resume run {own} to manage its sessions",
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The construction-oriented workflow object (finding 38): one
    #[tool(
        name = "tui_workflow",
        description = "Construction workflow per finding: inspect (the full chain — component identity, source loci, framework context, contract expectation, minimal reproduction, targeted validation), verify (run the finding's verification plan live: replay the reproduction and report whether the finding still reproduces; lease-gated), diagnose (all findings' chains). Joins existing evidence; never invents."
    )]
    pub async fn tui_workflow(
        &self,
        p: Parameters<TuiWorkflowParams>,
    ) -> rmcp::model::CallToolResult {
        crate::mcp::tools::handlers::workflow::tui_workflow(self, p).await
    }

    /// Coverage (spec section 5; Wave F item 64). Two providers, merged:
    /// the optional tuicov executable and the NativeSemanticProtocol
    /// coverage events the run ledger accumulates.
    #[tool(
        name = "tui_coverage",
        description = "Native coverage: run ledger (native events) plus optional tuicov executable. Actions: detect, summary, collect, delta (since_seq cursor), ledger, snapshot (requires tuicov on PATH; unsupported error otherwise). 'uncovered' is explicitly unsupported — there is no denominator of what the app COULD cover; do not call it."
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
        description = "Detect the TUI framework (detect), report backend capability probes and the NativeSemanticProtocol channel status (capabilities), and fetch adapter snippets (adapter_snippet; Ratatui/Textual/Python)."
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
        let run_id = self.run.lock().unwrap().id().to_string();
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
