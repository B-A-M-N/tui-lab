//! The central machine-driving pipeline (audit P0-1).
//!
//! `tui_act` was the only driver that ran the full evidence pipeline —
//! guarded canonical execution, frame commits, ledger transaction,
//! scenario capture, event/coverage folding. Every other driver (intent,
//! probe stimulus, scenario replay, exploration, audit drivers)
//! re-implemented a SUBSET, so each one silently dropped some invariant
//! the act path enforced. This module is the one pipeline:
//!
//! ```text
//! canonical execute (guard validated atomically with the send)
//!   → commit before/after frames
//!   → transaction ledger record
//!   → scenario capture (sensitivity-preserving)
//!   → fold session events (persistence + native coverage)
//! ```
//!
//! Authorization (the human-control lease) and envelope error mapping
//! stay one layer up (`mcp::tools::handlers::drive`), because they speak
//! the MCP error vocabulary; everything that can change the target TUI
//! funnels through here so the invariants stop being something handlers
//! remember.

use crate::execution::CanonicalAction;
use crate::run::RunContext;
use rmcp::serde_json::json;

/// The authorized run identity (beta-audit P0-6): the run id captured when
/// an actor job is QUEUED (the same lock window `with_sess`'s closed-run +
/// ownership guards read), checked again at every EVIDENCE COMMIT. Between
/// those two moments `tui_run new`/`resume` can swap the shared run
/// context; without the check, a transaction authorized under run A could
/// land in run B's ledger — foreign evidence spilling across a run switch,
/// the exact invariant the ownership map exists to protect.
///
/// The ticket names what to do: the evidence is DROPPED (never committed
/// to the wrong run) and the caller reports `run_switched`.
#[derive(Debug, Clone)]
pub struct RunTicket {
    pub(crate) run_id: String,
}

impl RunTicket {
    /// Capture at authorization time (with the same lock that decides the
    /// session's authorization).
    pub fn capture(run: &std::sync::Mutex<RunContext>) -> Self {
        RunTicket {
            run_id: run.lock().unwrap().id().to_string(),
        }
    }

    /// Verify at commit time. `Err` names both identities.
    pub fn verify(&self, run: &RunContext) -> Result<(), String> {
        if run.id() == self.run_id {
            Ok(())
        } else {
            Err(format!(
                "run switched under an in-flight operation: authorized under run '{}', current run is '{}'; the evidence was DROPPED, not committed to the wrong run",
                self.run_id,
                run.id()
            ))
        }
    }
}

/// Scenario capture for one driven act: the wire-shape request, so a
/// recording replays the exact step the caller sent. Sensitive acts are
/// recorded as `${PARAM}` references (payload stripped) — the same policy
/// `tui_act` has always applied, now applied by the boundary instead of
/// by each driver.
#[derive(Debug, Clone)]
pub struct ScenarioCapture {
    /// The act step params, in the `TuiActRequest` wire grammar.
    pub params: serde_json::Value,
    /// Whether the caller marked the payload sensitive.
    pub sensitive: bool,
}

/// Everything one driven act needs, beyond the action itself.
pub struct DriveSpec<'a> {
    pub action: &'a CanonicalAction,
    pub quiet_ms: u64,
    pub budget_ms: u64,
    pub no_wait: bool,
    pub visibility: crate::execution::InputVisibility,
    pub completion: crate::capture::CompletionPolicy,
    pub guard: Option<&'a crate::execution::MutationGuard>,
    /// `None` = do not touch scenario recordings (e.g. internal plan
    /// steps that are not themselves caller actions).
    pub scenario: Option<ScenarioCapture>,
    /// Finding 2: which subsystem is driving. Stamped on the transaction
    /// and the ledger row, so "WHO sent this input" is recorded evidence,
    /// not an inference from context.
    pub origin: crate::execution::DriveOrigin,
    /// Beta-audit P0-6: the run identity captured at authorization,
    /// verified at every commit.
    pub ticket: RunTicket,
}

/// Finding 9: how healthy the run-level evidence for one driven act is.
/// Frame commits and the ledger record CAN fail (run closed mid-drive,
/// disk full, append error) — before this model the failures were
/// `.unwrap_or_default()`-ed into frame id 0 and the outcome still cited
/// `"frame:0"` as if it were evidence. Health names exactly which
/// evidence legs committed and which did not, so a consumer citing the
/// run knows what stands behind it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvidenceHealth {
    /// Per-frame commit result, in [before, after] order: `Ok(frame_id)`
    /// or `Err(reason)`.
    pub frame_commits: [Result<u64, String>; 2],
    /// Whether the interaction transaction was recorded in the run
    /// ledger (the reconstructable record; sensitive payloads redacted).
    pub ledger_recorded: bool,
}
impl EvidenceHealth {
    /// True only when every evidence leg committed. A `false` here does
    /// not invalidate the act — the TUI still received the input (the
    /// execution itself succeeded) — it means the run's citable record
    /// is incomplete and the response says so.
    pub fn healthy(&self) -> bool {
        self.frame_commits.iter().all(|r| r.is_ok()) && self.ledger_recorded
    }

    /// Human-readable list of what failed, for warnings surfaces.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        let names = ["before", "after"];
        for (i, r) in self.frame_commits.iter().enumerate() {
            if let Err(reason) = r {
                out.push(format!("{} frame not committed: {reason}", names[i]));
            }
        }
        if !self.ledger_recorded {
            out.push("interaction not recorded in the run ledger".to_string());
        }
        out
    }

    /// The wire shape: committed ids stay `"frame:N"`; failed legs are
    /// `null` with the reason beside them, never a fabricated default id.
    pub fn frames_json(&self) -> serde_json::Value {
        let leg = |r: &Result<u64, String>| match r {
            Ok(id) => json!({ "ref": format!("frame:{id}") }),
            Err(reason) => json!({ "ref": serde_json::Value::Null, "error": reason }),
        };
        json!({
            "before": leg(&self.frame_commits[0]),
            "after": leg(&self.frame_commits[1]),
        })
    }
}

/// The evidence one driven act produced.
pub struct DriveOutcome {
    /// The full interaction transaction (frames, settle, transition,
    /// render evidence).
    pub tx: crate::execution::InteractionTransaction,
    /// Frame references: `{"before": "frame:N", "after": "frame:M"}`.
    pub frames: serde_json::Value,
    /// Finding 9: how much of that evidence actually committed to the
    /// run. The act succeeding and the evidence committing are separate
    /// facts; both are reported.
    pub health: EvidenceHealth,
}

/// Beta-audit P0-7: the ONE evidence sink every driving path commits
/// through. Authorized dispatch (`with_sess_authorized`, or an evidence
/// -carrying driver entry) installs it on the session; the canonical
/// executor's tail commits EVERY transaction it produces through
/// [`Self::commit`] and folds the session's events through
/// [`Self::fold`]. That makes `drive_pipeline`'s invariants — typed
/// origin, citable frames, reconstructable ledger row, event/coverage
/// fold, ticket-verified run identity — properties of the executor, not
/// things each driver (exploration, audit drivers, repro, conformance,
/// probe) has to remember.
///
/// The run Arc is locked BRIEFLY per commit, never held across driving —
/// an exploration that used to hold the run mutex for its whole loop now
/// only holds it for the per-act commit window.
#[derive(Clone)]
pub struct RunEvidenceSink {
    run: std::sync::Arc<std::sync::Mutex<RunContext>>,
    ticket: RunTicket,
    /// Health of the most recent commit (finding 9): drivers that do not
    /// return per-act outcomes (audit drivers, exploration) can still
    /// report — and tests can assert — what actually committed.
    last_health: std::sync::Arc<std::sync::Mutex<Option<EvidenceHealth>>>,
}

impl RunEvidenceSink {
    /// Capture at authorization: the run identity is read under the same
    /// lock window that decided the dispatch.
    pub fn capture(run: &std::sync::Arc<std::sync::Mutex<RunContext>>) -> Self {
        RunEvidenceSink {
            run: run.clone(),
            ticket: RunTicket::capture(run),
            last_health: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// An explicit ticket over an existing Arc (tests, non-standard
    /// authorization windows).
    pub fn with_ticket(
        run: std::sync::Arc<std::sync::Mutex<RunContext>>,
        ticket: RunTicket,
    ) -> Self {
        RunEvidenceSink {
            run,
            ticket,
            last_health: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn ticket(&self) -> &RunTicket {
        &self.ticket
    }

    /// Finding 9: how healthy the most recent commit through this sink
    /// was. `None` = nothing committed yet.
    pub fn last_health(&self) -> Option<EvidenceHealth> {
        self.last_health.lock().unwrap().clone()
    }

    /// Commit one interaction transaction: ticket verify → frames →
    /// ledger. `Err` names a run-switch refusal (evidence DROPPED, never
    /// committed to the wrong run). Frame/ledger failures are RECORDED
    /// (finding 9), not fatal — the act already happened; health says
    /// which legs stand behind it.
    pub fn commit(
        &self,
        sid: &str,
        gen: u32,
        tx: &crate::execution::InteractionTransaction,
    ) -> Result<EvidenceHealth, anyhow::Error> {
        let mut run = self.run.lock().unwrap();
        // Beta-audit P0-6: a run switch between authorization and commit
        // aborts the whole evidence commit — the transaction belongs to
        // the run that authorized it, and that run is gone.
        self.ticket.verify(&run).map_err(anyhow::Error::msg)?;
        // Frame commit pipeline (re-review item 40): both frames through
        // the ONE commit path — id + provenance + incremental append.
        let mut commit = |f: &crate::backend::CanonicalFrame| {
            let mut f = f.clone();
            f.session_id = Some(sid.to_string());
            f.generation = Some(gen);
            run.commit_frame(&mut f, Some(sid))
                .map_err(|e| e.to_string())
        };
        let frame_commits = [commit(&tx.before_frame), commit(&tx.after_frame)];
        // Run ledger (Wave-2 item 15): the reconstructable transaction
        // record. Sensitive payloads are projected to Redacted(kind,
        // byte_len) by the ledger itself.
        let ledger_recorded = run.record_interaction(sid, tx).is_ok();
        let health = EvidenceHealth {
            frame_commits,
            ledger_recorded,
        };
        *self.last_health.lock().unwrap() = Some(health.clone());
        Ok(health)
    }

    /// Universal evidence fold (audit §28): the session's event queue
    /// lands in the run (incremental persistence + native coverage). A
    /// run switch between authorization and fold drops the fold — the
    /// events stay queued for a later fold under the right run.
    pub fn fold(&self, sess: &mut crate::session::Session) {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return; // run switched mid-operation; skip this fold
        }
        fold_into_run(sess, &mut run);
    }

    // ── Beta-audit P0.3: typed ticket-verified bookkeeping ops ──────
    //
    // Handlers must NOT grab `Arc<Mutex<RunContext>>` for run-side
    // bookkeeping: the sink owns ticket verification once, here, so no
    // subsystem has to remember which calls need it. Every op verifies
    // the ticket and returns `false` on a run switch (evidence dropped
    // — the caller reports `run_switched`, never misattributes).

    /// Record one event marker for `sid`.
    pub fn record_event(&self, sid: &str, event: &str) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_event(sid, event).is_ok()
    }

    /// Record one scenario act step, generation-scoped.
    pub fn record_scenario_act(&self, sid: &str, gen: u32, params: serde_json::Value) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_scenario_act(sid, gen, params).is_ok()
    }

    /// Record one scenario act step with a redacted sensitive payload.
    #[allow(clippy::too_many_arguments)]
    pub fn record_scenario_act_sensitive(
        &self,
        sid: &str,
        gen: u32,
        params: serde_json::Value,
        field: &str,
        kind: crate::scenario::model::SensitiveKind,
        payload_bytes: usize,
    ) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_scenario_act_sensitive(sid, gen, params, field, kind, payload_bytes)
            .is_ok()
    }

    /// Record one scenario assert step, generation-scoped.
    pub fn record_scenario_assert(&self, sid: &str, gen: u32, params: serde_json::Value) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_scenario_assert(sid, gen, params).is_ok()
    }

    /// Record one scenario wait step, generation-scoped.
    pub fn record_scenario_wait(&self, sid: &str, gen: u32, params: serde_json::Value) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_scenario_wait(sid, gen, params).is_ok()
    }

    /// Record one first-class scenario intent step, generation-scoped.
    pub fn record_scenario_intent(&self, sid: &str, gen: u32, params: serde_json::Value) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.record_scenario_intent(sid, gen, params).is_ok()
    }

    /// Merge a locally-accumulated exploration state graph into the
    /// run's (idempotent merge; a run switch drops the merge).
    pub fn merge_state_graph(&self, local: &crate::exploration::state_graph::StateGraph) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.graphs_mut().state_graph.merge(local);
        true
    }

    /// Merge a locally-accumulated focus graph into the run's.
    pub fn merge_focus_graph(&self, local: &crate::semantic::focus_graph::FocusGraph) -> bool {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return false;
        }
        run.graphs_mut().focus_graph.merge(local);
        true
    }

    /// Run one read-or-write closure against the ticketed run. This is
    /// the ESCAPE HATCH for bookkeeping families the typed ops don't
    /// cover yet (checkpoints today) — it still funnels through the one
    /// ticket verification, so a run switch makes the closure see
    /// `None` and the caller reports dropped evidence instead of
    /// writing into the wrong run. Prefer a typed op when one exists;
    /// new bookkeeping families should extend the typed surface, not
    /// reach for the raw `Arc<Mutex<RunContext>>` in handlers.
    pub fn with_run<R>(&self, f: impl FnOnce(&mut RunContext) -> R) -> Option<R> {
        let mut run = self.run.lock().unwrap();
        if self.ticket.verify(&run).is_err() {
            return None;
        }
        Some(f(&mut run))
    }
}

/// THE driving pipeline. Runs inside the session's actor (call it from a
/// `with_sess` closure only): guarded execute → frames → ledger →
/// scenario → event/coverage fold. Beta-audit P0-6: `ticket` is the run
/// identity captured at authorization; every commit below verifies it, so
/// a run switch mid-drive drops the evidence instead of spilling it into
/// the new run.
pub fn drive(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<RunContext>>,
    spec: DriveSpec<'_>,
) -> Result<DriveOutcome, anyhow::Error> {
    let tx = crate::execution::execute_act_with_guard_and_origin(
        sess,
        spec.origin,
        spec.action,
        spec.quiet_ms,
        spec.budget_ms,
        spec.no_wait,
        spec.visibility,
        spec.completion,
        spec.guard,
    )?;
    // Beta-audit P0-7: the commit + fold ride the ONE sink path. When the
    // executor already committed through the session's installed sink
    // (authorized dispatch — same run Arc, verified ticket), that commit
    // IS the evidence; a second one would double-book the ledger row.
    // Only a caller that dispatched without an installed sink commits
    // here (tests, non-standard entry points — still verified).
    let installed = sess.evidence_sink();
    let already_committed = installed.as_ref().is_some_and(|s| {
        std::sync::Arc::ptr_eq(&s.run, run) && s.ticket.run_id == spec.ticket.run_id
    });
    let sink = installed.unwrap_or_else(|| {
        std::sync::Arc::new(RunEvidenceSink {
            run: run.clone(),
            ticket: spec.ticket.clone(),
            last_health: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    });
    let (sid, gen) = (sess.id.clone(), sess.generation);
    let health = if already_committed {
        sink.last_health()
            .ok_or_else(|| anyhow::anyhow!("sink reported no commit health"))?
    } else {
        sink.commit(&sid, gen, &tx)?
    };
    // Scenario capture (re-review P0.3): sensitive text payloads are
    // recorded as ${PARAM} references; sensitive non-text payloads as
    // an opaque redacted step; everything else verbatim.
    // Beta-audit P0.3: through the sink's ticket-verified ops — the
    // scenario step belongs to the run that authorized the drive, and
    // a run switch drops it instead of misattributing it.
    if let Some(cap) = &spec.scenario {
        let recorded = if cap.sensitive {
            match spec.action {
                CanonicalAction::Type { .. } => Some("text"),
                CanonicalAction::Paste { .. } => Some("paste"),
                _ => None,
            }
        } else {
            None
        };
        match recorded {
            Some(field) => {
                sink.record_scenario_act_sensitive(
                    &sid,
                    gen,
                    cap.params.clone(),
                    field,
                    crate::scenario::model::SensitiveKind::Secret,
                    spec.action.payload_len(),
                );
            }
            None if cap.sensitive => {
                sink.record_scenario_act(
                    &sid,
                    gen,
                    json!({
                        "action": spec.action.name(),
                        "sensitive": true,
                        "redacted": true,
                        "payload_bytes": spec.action.payload_len(),
                    }),
                );
            }
            None => {
                sink.record_scenario_act(&sid, gen, cap.params.clone());
            }
        }
    }
    // Universal evidence fold (audit §28): the session's event queue lands
    // in the run (incremental persistence + native coverage) after EVERY
    // driven act, not only on observe sweeps. Idempotent with the
    // executor's own fold (consumer cursors).
    sink.fold(sess);
    let frames = health.frames_json();
    Ok(DriveOutcome { tx, frames, health })
}

/// Fold the session's event queue into the run: incremental event
/// persistence via the run's own consumer cursor, then native coverage
/// ingestion. Extracted from the observe `sweep` so every driving path
/// shares one fold; idempotent per event (seqs are unique).
/// Beta-audit P0-6: a run switch between authorization and fold drops
/// the fold (events stay in the session queue; a later fold under the
/// right run picks them up — nothing is lost or misrouted).
pub fn fold_session_events(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<RunContext>>,
    ticket: &RunTicket,
) {
    let mut run = run.lock().unwrap();
    if ticket.verify(&run).is_err() {
        return; // run switched mid-operation; skip this fold
    }
    fold_into_run(sess, &mut run);
}

/// The fold body, shared by [`fold_session_events`] and
/// [`RunEvidenceSink::fold`] — caller already holds the run lock and has
/// verified the ticket.
fn fold_into_run(sess: &mut crate::session::Session, run: &mut RunContext) {
    {
        let cursor_key = format!("persistence:{}", sess.id);
        let from = run.event_cursor(&cursor_key).unwrap_or(0);
        let batch = sess.events_since(from);
        if !batch.events.is_empty() {
            let _ = run.hold_events(&sess.id, batch.events);
            run.set_event_cursor(&cursor_key, batch.cursor);
        }
    }
    run.bump_event();
    // Coverage ingestion rides every fold (re-review P1 item 17): native
    // coverage events were absorbed into the session queue by observe
    // itself, so the run ledger folds them here.
    let batch = sess.events_since(0);
    let cursor_key = format!("coverage:{}", sess.id);
    let from = run.event_cursor(&cursor_key).unwrap_or(0);
    let seq = batch.cursor;
    if batch.cursor > from {
        let evs = sess.events_since(from);
        // Wave 5 item 41: widget-targeted coverage events join the
        // declaring node's app-attested source locus (same NSP channel),
        // so coverage→source is exact. File targets stay plain.
        for ev in &evs.events {
            if let crate::events::TerminalEventKind::NativeEvent { event, target } = &ev.kind {
                if event != "coverage" {
                    continue;
                }
                let locus = sess.native_source_for_widget(target).filter(|_| {
                    !(target.starts_with("src/") || target.contains(".rs:") || target.contains('/'))
                });
                match locus {
                    Some(sr) => {
                        let _ = run.record_coverage_event_with_identity(&sess.id, target, sr);
                    }
                    None => {
                        let _ = run.record_coverage_event(&sess.id, target);
                    }
                }
            }
        }
        run.set_event_cursor(&cursor_key, seq);
    }
}

/// Serialize a [`CanonicalAction`] into the `TuiActRequest` wire grammar
/// scenario act steps replay. (The canonical enum's own serde uses
/// `{"kind":...}`; scenario act steps parse `{"action":...}` — this is
/// the one bridge.)
pub fn act_request_json(action: &CanonicalAction) -> serde_json::Value {
    use crate::backend::{MouseButton as MB, ScrollDirection as SD};
    fn button_name(b: MB) -> &'static str {
        match b {
            MB::Left => "left",
            MB::Middle => "middle",
            MB::Right => "right",
        }
    }
    fn direction_name(d: SD) -> &'static str {
        match d {
            SD::Up => "up",
            SD::Down => "down",
        }
    }
    match action {
        CanonicalAction::Key { key } => json!({ "action": "key", "key": key.display() }),
        CanonicalAction::Keys { keys } => json!({
            "action": "keys",
            "keys": keys.iter().map(|k| k.display()).collect::<Vec<_>>(),
        }),
        CanonicalAction::Type { text } => json!({ "action": "type", "text": text }),
        CanonicalAction::Paste { text } => json!({ "action": "paste", "paste": text }),
        CanonicalAction::Raw { bytes } => json!({ "action": "raw", "raw": bytes }),
        CanonicalAction::MouseClick { button, x, y } => {
            json!({ "action": "mouse_click", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MousePress { button, x, y } => {
            json!({ "action": "mouse_press", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseRelease { button, x, y } => {
            json!({ "action": "mouse_release", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseMove { x, y } => json!({ "action": "mouse_move", "x": x, "y": y }),
        CanonicalAction::MouseDrag { button, x, y } => {
            json!({ "action": "mouse_drag", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseScroll { direction, x, y } => json!({
            "action": "mouse_scroll", "direction": direction_name(*direction), "x": x, "y": y,
        }),
        CanonicalAction::Resize { cols, rows } => {
            json!({ "action": "resize", "cols": cols, "rows": rows })
        }
        CanonicalAction::Signal { signal } => json!({ "action": "signal", "signal": signal }),
    }
}
