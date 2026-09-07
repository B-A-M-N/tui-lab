//! tui_act / tui_intent / tui_checkpoint: canonical driving through the
//! shared [`super::drive`] boundary, and checkpoint comparison.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_invalid_selector, err_with_details, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

use super::drive::{drive, DriveSpec, ScenarioCapture};

/// Body of `tui_checkpoint` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_checkpoint(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiCheckpointParams>,
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
    // Beta-audit P0.3: checkpoint bookkeeping runs against the
    // ticketed run through the sink's verified escape hatch — a run
    // switch between admission and the checkpoint write reports the
    // operation as refused (run_switched) instead of storing the
    // checkpoint into the new run.
    let sink = crate::execution::RunEvidenceSink::capture(&s.run);
    // The actor closure owns the session side (observe) and does the
    // checkpoint work against the run under a short lock.
    s.with_sess(p.id.as_deref(), move |sess| {
        let session_id = sess.id.clone();
        let generation = sess.generation;
        match ckpt_action {
            CA::List => match sink.with_run(|run| run.checkpoints.list(&session_id)) {
                Some(checkpoints) => ok(json!({ "checkpoints": checkpoints })),
                None => run_switched(),
            },
            CA::Save => {
                let (screen, sem, _, _) = match sess.observe_fused(40) {
                    Ok(t) => t,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                match sink.with_run(|run| {
                    run.checkpoints.save(
                        &session_id,
                        generation,
                        p.name.clone(),
                        &screen,
                        Some(&sem),
                    )
                }) {
                    Some(name) => ok(json!({
                        "name": name,
                        "structure_hash": screen.structure_hash,
                        "visual_hash": screen.visual_hash,
                        "focus": sem.focus.control,
                        "controls": sem.controls.len(),
                    })),
                    None => run_switched(),
                }
            }
            CA::Compare => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "compare requires 'name'"),
                };
                let (screen, sem, _, _) = match sess.observe_fused(40) {
                    Ok(t) => t,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                match sink.with_run(|run| {
                    run.checkpoints
                        .compare(&session_id, &name, &screen, Some(&sem))
                }) {
                    Some(Ok(json_out)) => crate::mcp::helpers::ok_from_json(&json_out),
                    Some(Err(ErrorCategory::InvalidRequest)) => {
                        err(ErrorCategory::InvalidRequest, "no such checkpoint")
                    }
                    Some(Err(c)) => err(c, "checkpoint comparison failed"),
                    None => run_switched(),
                }
            }
            CA::Delete => {
                let name = match &p.name {
                    Some(n) => n.clone(),
                    None => return err(ErrorCategory::InvalidRequest, "delete requires 'name'"),
                };
                match sink.with_run(|run| run.checkpoints.delete(&session_id, &name)) {
                    Some(removed) => ok(json!({ "name": name, "deleted": removed })),
                    None => run_switched(),
                }
            }
        }
    })
    .await
    .unwrap_or_else(|e| e)
}

/// The refusal for a bookkeeping operation whose run era ended between
/// admission and the run-side write (beta-audit P0.3): the operation is
/// rejected, never misattributed. Mirrors the ticket's own commit
/// language so callers see one consistent story.
fn run_switched() -> rmcp::model::CallToolResult {
    err(
        ErrorCategory::RunClosed,
        "run switched under an in-flight operation: the checkpoint bookkeeping was REFUSED (not committed to the wrong run); resume or re-issue against the current run",
    )
}

/// Body of `tui_act` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_act(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiActRequest>,
) -> rmcp::model::CallToolResult {
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
    // Quiet precedence (re-review P0.1): an explicit `wait_ms` wins,
    // then a completion spec's own `quiet_ms` ({"type":"stable_screen",
    // "quiet_ms":400} is s-contained), then the 150ms default.
    let quiet = p
        .wait_ms()
        .or_else(|| p.completion_quiet_ms())
        .unwrap_or(150);
    let selector = p.id().map(str::to_string);
    let run = s.run.clone();
    // Review P0 rigidity #4 + re-review P0.10: an agent may declare how
    // "done" means for this action — EVERY canonical action, Resize and
    // Signal included. A declared completion overrides the default
    // "screen settles" so silent/exit/signal actions are classified
    // honestly (never a false `settled=false`).
    let completion = p
        .completion()
        .unwrap_or(crate::capture::CompletionPolicy::StableScreen);
    // Beta-audit P0-6: the authorized entry — the run ticket is captured
    // under the same lock window as with_sess's closed-run + ownership
    // guards, and every evidence commit verifies it.
    s.with_sess_authorized(selector.as_deref(), move |sess, ticket| {
            // The ONE driving pipeline (audit P0-1): lease refusal, guarded
            // canonical execution, frame commits, ledger record, scenario
            // capture, event/coverage fold — all in the shared boundary.
            let outcome = match drive(
                sess,
                &run,
                DriveSpec {
                    action: &action,
                    quiet_ms: quiet,
                    budget_ms: quiet.saturating_add(1000),
                    no_wait: p.no_wait(),
                    visibility,
                    completion,
                    // Re-review P0.9: an agent-declared expected-state guard is
                    // validated atomically with the send. Drift refuses the
                    // action with a structured stale_state verdict instead of
                    // landing input in a changed UI.
                    guard: p.guard().map(|g| g.to_guard()).as_ref(),
                    scenario: Some(ScenarioCapture {
                        params: serde_json::to_value(&p).unwrap_or_default(),
                        sensitive,
                    }),
                    origin: crate::execution::DriveOrigin::Act,
                    ticket: Some(ticket),
                },
            ) {
                Ok(o) => o,
                Err(refused) => return refused,
            };
            let tx = outcome.tx;
            // Causal render evidence (re-review item 19): the exact
            // protocol bytes the action produced, decoded — the answer to
            // "pressing Down caused WHICH escape sequences?".
            let render = tx.render.as_ref().map(|r| {
                json!({
                    "action": r.action,
                    "protocol_range": { "start": r.range_start, "end": r.range_end, "bytes": r.bytes },
                    "complete": r.complete,
                    "op_count": r.op_count,
                    "ops": r.ops.iter().map(|o| json!({ "at": o.at, "op": o.describe })).collect::<Vec<_>>(),
                    "first_byte_ms": r.first_byte_ms,
                    "first_frame_ms": r.first_frame_ms,
                    "first_semantic_ms": r.first_semantic_ms,
                    "full_repaint_ratio": r.full_repaint_ratio,
                    "dirty_cells": r.dirty_cells,
                    "dirty_rows": r.dirty_rows,
                    "note": if r.complete { "" } else {
                        "the raw ring evicted this action's bytes; the range is the citable evidence, ops are unavailable"
                    },
                })
            });
            // Finding 9: the act landing and the evidence committing are
            // separate facts. Citable frames carry ids; failed legs are
            // null + error, and `evidence_health.healthy: false` names the
            // caveat explicitly instead of letting "frame:0" impersonate
            // evidence.
            let health = &outcome.health;
            let mut warnings = if tx.settled() {
                Vec::<String>::new()
            } else if tx.settle == crate::execution::SettleStatus::Skipped {
                vec!["settlement was not tested (no_wait=true); reported honestly as skipped".to_string()]
            } else {
                vec!["screen did not reach the requested stability within the settle budget".to_string()]
            };
            warnings.extend(health.failures());
            ok(json!({
                "action": tx.name(),
                // Finding 2: typed provenance — this response came from the
                // `act` driver; every driver tags its acts the same way and
                // the ledger row carries the same slug.
                "origin": tx.origin.map(|o| o.as_str()),
                "settled": tx.settled(),
                "settle_status": tx.settle,
                "settle_reason": tx.settle_reason(),
                "elapsed_ms": tx.elapsed_ms,
                "frames": outcome.frames,
                "evidence_health": {
                    "healthy": health.healthy(),
                    "ledger_recorded": health.ledger_recorded,
                    "failures": health.failures(),
                },
                "warnings": warnings,
                "render": render,
                "transition": tx.transition,
            }))
        })
        .await
        .unwrap_or_else(|e| e)
}

/// Body of `tui_intent`: resolve a semantic target + verb into a
/// focus-secured execution plan (review item — the intent system was
/// engine-complete but had no MCP surface). Plan-only by default: the
/// response names the exact steps and the risk class BEFORE anything is
/// sent. `execute=true` re-resolves against a FRESH observation (audit
/// P0-13: the cached frame's geometry can predate a UI change, and the
/// EnsureFocus click would land on stale coordinates), then runs every
/// step through the shared [`super::drive`] boundary (audit P0-12: intent
/// actions are evidenced exactly like `tui_act` — frames, ledger
/// transactions, scenario steps — under one `intent:<id>` linkage record).
pub(crate) async fn tui_intent(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiIntentParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    let verb = match p.verb.parse() {
        Ok(v) => v,
        Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
    };
    let execute = p.execute.unwrap_or(false);
    let sensitive = p.sensitive.unwrap_or(false);
    let visibility = if sensitive {
        crate::execution::InputVisibility::Sensitive
    } else {
        crate::execution::InputVisibility::Normal
    };
    // Completion override (audit P0-15): a caller-supplied spec governs
    // the payload action; the default stays stable-screen.
    let completion = p
        .completion
        .as_ref()
        .map(|c| c.to_policy())
        .unwrap_or(crate::capture::CompletionPolicy::StableScreen);
    let intent_id = format!("intent-{}", uuid::Uuid::new_v4().simple());
    let selector = p.id.clone();
    let target = p.target.clone();
    // Beta-audit P0-9: the wire verb, kept for the recorded intent step
    // (the scenario grammar round-trips this exact shape).
    let intent_verb_json = serde_json::to_value(&p.verb).unwrap_or(serde_json::Value::Null);
    let run = s.run.clone();
    // Finding 3D: the plan store lives on the server; clone the Arc handle
    // into the closure instead of capturing `s` (whose borrow cannot cross
    // the actor await).
    let intent_plans = s.plans_handle();
    // Beta-audit P0-6: execution drives the TUI and commits evidence, so
    // it uses the authorized entry (ticket captured with the guards).
    // Planning is observation-only; it still gets the ticket so both arms
    // share one closure shape.
    s.with_sess_authorized(selector.as_deref(), move |sess, ticket| {
        // Beta-audit P0-8: execute=true without a plan_id is a request-shape
        // error — refused BEFORE any observation or target resolution, so
        // the two-step contract is enforced with zero side effects.
        if execute && p.plan_id.is_none() {
            return err_with_details(
                ErrorCategory::InvalidRequest,
                "execute=true requires the plan_id returned by the preview (execute=false): pass the plan_id you inspected, or re-plan to get one",
                json!({ "reason": "plan_id_required" }),
            );
        }
        // Planning reads the session's LAST fused frame (observe-before-act):
        // the semantic truth the agent would have seen from tui_observe.
        // Execution below re-observes fresh instead.
        let plan_frame = if execute {
            // Audit P0-13: fresh observation BEFORE any focus move. The plan the
            // agent saw may be stale; this frame is what actually executes.
            match sess.observe_fused(40) {
                Ok((_, sem, _, _)) => Some(sem),
                Err(e) => {
                    return err(ErrorCategory::BackendError, format!("pre-execute observe failed: {e}"))
                }
            }
        } else {
            sess.analyze_last().map(|a| a.semantic)
        };
        let Some(semantic) = plan_frame else {
            return err(
                ErrorCategory::InvalidRequest,
                "no observed frame yet — observe the session (tui_observe) before planning an intent, or the target has nothing to resolve against",
            );
        };
        // Finding 3: focus routes are planned from the run's PROVEN
        // FocusGraph (real observed Tab/Shift+Tab transitions), never from
        // guesses. An empty graph makes focus-needing verbs honestly
        // Unsupported with the remedy named.
        let graph_snapshot = {
            let run = run.lock().unwrap();
            run.graphs().focus_graph.clone()
        };
        let plan = match crate::intent::plan_intent_with_graph(
            &semantic,
            &target,
            verb.clone(),
            &graph_snapshot,
        ) {
            Ok(pl) => pl,
            // Target resolution failures are refinement-loop payloads, not
            // malformed requests: category=target_error with the
            // candidates/matches structured in `details` (review §14).
            Err(e) => {
                let details = match &e {
                    crate::intent::IntentError::Ambiguous { matches, .. } => {
                        json!({ "reason": "ambiguous_target", "matches": matches })
                    }
                    crate::intent::IntentError::NotFound { candidates, .. } => {
                        json!({ "reason": "target_not_found", "candidates": candidates })
                    }
                    crate::intent::IntentError::VerbMismatch { target, verb } => {
                        json!({ "reason": "verb_mismatch", "target": target, "verb": verb })
                    }
                    crate::intent::IntentError::Unsupported { target, verb, why } => {
                        json!({ "reason": "unsupported_intent", "target": target, "verb": verb, "why": why })
                    }
                };
                return err_with_details(ErrorCategory::TargetError, e.message(), details);
            }
        };
        let plan_json = plan_to_json(&plan);
        if !execute {
            // Finding 3D + beta-audit P0-8: the plan response carries a
            // plan_id so execution can reference THIS previewed plan. The
            // ticket binds the FULL plan fingerprint — control, session,
            // verb, risk, and the exact step shape — not just the control
            // identity, so a caller cannot preview verb A and submit the
            // plan_id for verb B against the same control.
            let plan_id = {
                let mut plans = intent_plans.lock().unwrap();
                let id = format!("plan-{}", uuid::Uuid::new_v4().simple());
                plans.store(
                    id.clone(),
                    crate::mcp::ownership::IntentPlanTicket {
                        control_id: plan.control.id.clone(),
                        session_id: sess.id.clone(),
                        verb: plan.verb.name().to_string(),
                        risk: plan.risk.name().to_string(),
                        steps_hash: crate::mcp::ownership::plan_steps_hash(&plan_json["steps"]),
                        created: std::time::Instant::now(),
                    },
                );
                id
            };
            return ok(json!({
                "mode": "planned",
                "plan_id": plan_id,
                "control": plan_json["control"],
                "verb": plan.verb.name(),
                "risk": plan.risk.name(),
                "steps": plan_json["steps"],
                "note": "plan only — nothing was sent; execute by passing execute=true with this plan_id (plus max_risk when the plan is destructive/external/unknown)",
            }));
        }
        // ── Finding 3D: authorization gates, BEFORE anything is sent ──
        // 1. Risk fence: a plan whose risk exceeds max_risk is refused.
        // Destructive/external/unknown ALWAYS need an explicit covering
        // fence — planning alone never authorizes them.
        if plan.risk.needs_confirmation() && p.max_risk.is_none() {
            return err_with_details(
                ErrorCategory::InvalidRequest,
                format!(
                    "plan risk '{}' requires an explicit max_risk fence before execution — planning alone does not authorize {} risk",
                    plan.risk.name(),
                    plan.risk.name()
                ),
                json!({
                    "reason": "risk_fence_required",
                    "plan_risk": plan.risk.name(),
                    "expected": format!("max_risk at or above the plan's risk, e.g. \"max_risk\": \"{}\"", plan.risk.name()),
                }),
            );
        }
        if let Some(fence) = p.max_risk.as_deref() {
            match crate::intent::ActionRisk::parse(fence) {
                None => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "invalid max_risk '{fence}' (expected one of: safe, mutating, destructive, external_side_effect, unknown)"
                        ),
                    )
                }
                Some(ceiling) => {
                    if plan.risk > ceiling {
                        return err_with_details(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "plan risk '{}' exceeds the max_risk fence '{}'; nothing was sent",
                                plan.risk.name(),
                                ceiling.name()
                            ),
                            json!({
                                "reason": "risk_fence",
                                "plan_risk": plan.risk.name(),
                                "max_risk": ceiling.name(),
                            }),
                        );
                    }
                }
            }
        }
        // 2. plan_id revalidation (finding 3D + beta-audit P0-8): the
        // caller must be executing the plan it ACTUALLY previewed —
        // plan_id is REQUIRED for execution, not an optional nicety. The
        // stored fingerprint (control, session, verb, risk, step shape)
        // must match the fresh resolution in full; any drift (UI change,
        // different verb, different session) refuses instead of acting on
        // a lookalike.
        // (plan_id's presence was enforced above — P0-8; here it is only
        // unwrapped for the fingerprint check.)
        let want = p.plan_id.as_deref().expect("plan_id enforced above");
        match intent_plans.lock().unwrap().consume(want) {
            Some(stored) => {
                let fresh_id = plan_json["control"]["id"].as_str().unwrap_or_default();
                let fresh_verb = plan.verb.name();
                let fresh_risk = plan.risk.name();
                let fresh_hash = crate::mcp::ownership::plan_steps_hash(&plan_json["steps"]);
                if stored.session_id != sess.id {
                    return err_with_details(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "plan_id '{want}' was previewed against session '{}', not '{}'; re-plan against the target session",
                            stored.session_id,
                            sess.id
                        ),
                        json!({
                            "reason": "plan_session_mismatch",
                            "plan_id": want,
                            "previewed_session": stored.session_id,
                        }),
                    );
                }
                if !stored.fingerprint(fresh_id, fresh_verb, fresh_risk, fresh_hash) {
                    return err_with_details(
                        ErrorCategory::StaleState,
                        format!(
                            "the executed request no longer matches the previewed plan '{want}' (previewed control '{}' / verb '{}' / risk '{}'; fresh resolution: '{fresh_id}' / '{fresh_verb}' / '{fresh_risk}'); re-plan instead of executing a lookalike",
                            stored.control_id, stored.verb, stored.risk
                        ),
                        json!({
                            "reason": "stale_plan",
                            "plan_id": want,
                            "previewed_control": stored.control_id,
                            "previewed_verb": stored.verb,
                            "previewed_risk": stored.risk,
                            "fresh_control": fresh_id,
                            "fresh_verb": fresh_verb,
                            "fresh_risk": fresh_risk,
                        }),
                    );
                }
            }
            None => {
                return err_with_details(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "unknown, expired, or already-executed plan_id '{want}'; plans live server-side for {}s and execute at most once — re-plan (execute=false) and execute the fresh plan_id",
                        crate::mcp::ownership::INTENT_PLAN_TTL.as_secs()
                    ),
                    json!({ "reason": "unknown_plan", "plan_id": want }),
                );
            }
        }
        // A live human lease blocks execution (planning stays allowed — it
        // sends nothing). Checked here AND inside `drive` per action.
        if let Some(refused) = crate::mcp::helpers::lease_refused(sess) {
            return refused;
        }
        // Execute the plan's steps in order, EVERY send through the shared
        // boundary (audit P0-12): frames, ledger, scenario capture, event
        // fold — with the plan's steps linked under one intent id.
        let mut pending_focus_guard: Option<String> = None;
        let mut executed: Vec<serde_json::Value> = Vec::new();
        for step in &plan.steps {
            match step {
                // Finding 3B: the focus move is a NON-activating traversal
                // key (Tab / Shift+Tab) from the proven route — never a
                // click, so the payload below is the ONLY activation.
                crate::intent::PlannedStep::MoveFocus { key, target_id } => {
                    // A generous quiet window: the redraw that proves
                    // the focus move landed must be committed before
                    // the AssertFocus guard reads it — a focus move
                    // that half-arrived SHOULD fail the plan, but not
                    // because we sampled before the app answered.
                    let focus_outcome = match drive(
                        sess,
                        &run,
                        DriveSpec {
                            action: key,
                            quiet_ms: 300,
                            budget_ms: 1300,
                            no_wait: false,
                            visibility: crate::execution::InputVisibility::Normal,
                            completion: crate::capture::CompletionPolicy::StableScreen,
                            guard: None,
                            // Internal plan step: evidenced in the ledger,
                            // but not a caller-authored scenario act.
                            scenario: None,
                            origin: crate::execution::DriveOrigin::Intent,
                            ticket: Some(ticket.clone()),
                        },
                    ) {
                        Ok(o) => o,
                        Err(refused) => return refused,
                    };
                    executed.push(json!({
                        "step": "move_focus", "how": key.signature(),
                        "target": target_id, "ok": true,
                        "frames": focus_outcome.frames,
                        "evidence_health": {
                            "healthy": focus_outcome.health.healthy(),
                            "failures": focus_outcome.health.failures(),
                        },
                    }));
                    // The executor's settle observes through the
                    // backend's wait path and does NOT refresh the
                    // session's last frame — the guard below reads
                    // `analyze_last()`, so observe once to make the
                    // post-move screen the one it validates against.
                    // Without this the guard would re-check the
                    // PRE-move screen and the plan could never pass
                    // its own focus assertion.
                    if let Err(e) = sess.observe(0) {
                        return err(ErrorCategory::BackendError, format!(
                            "post-focus observe failed: {e}"
                        ));
                    }
                }
                crate::intent::PlannedStep::AssertFocus { target_id } => {
                    pending_focus_guard = Some(target_id.clone());
                }
                crate::intent::PlannedStep::Act(action) => {
                    let guard = pending_focus_guard.take().map(|tid| {
                        crate::execution::MutationGuard {
                            focus_control_id: Some(tid),
                            ..Default::default()
                        }
                    });
                    let outcome = match drive(
                        sess,
                        &run,
                        DriveSpec {
                            action,
                            quiet_ms: 150,
                            budget_ms: 1150,
                            no_wait: false,
                            visibility,
                            completion: completion.clone(),
                            guard: guard.as_ref(),
                            // Beta-audit P0-9: the payload is recorded as
                            // the FIRST-CLASS intent step below (target +
                            // verb as semantic facts), NOT as a frozen act
                            // step — a recorded key sequence would replay
                            // against whatever control holds focus in a
                            // fresh session, which is exactly the defect
                            // the intent system exists to prevent.
                            scenario: None,
                            origin: crate::execution::DriveOrigin::Intent,
                            ticket: Some(ticket.clone()),
                        },
                    ) {
                        Ok(o) => o,
                        Err(refused) => return refused,
                    };
                    executed.push(json!({
                        "step": "act", "action": action.name(),
                        "signature": action.signature(),
                        "origin": outcome.tx.origin.map(|o| o.as_str()),
                        "settled": outcome.tx.settled(),
                        "settle": format!("{:?}", outcome.tx.settle).to_lowercase(),
                        "frames": outcome.frames,
                        "evidence_health": {
                            "healthy": outcome.health.healthy(),
                            "failures": outcome.health.failures(),
                        },
                    }));
                }
            }
        }
        // Finding 3 (plan enrichment): this execution's REAL focus
        // transitions (observed before/after each hop) are recorded into
        // the run's FocusGraph below by the drive boundary — here we only
        // write the intent linkage record (audit P0-12): one ledger entry
        // tying the plan's transactions together causally. Beta-audit
        // P0-9: scenario recordings also get the FIRST-CLASS intent step
        // — target + verb as semantic facts, not frozen keys — so replay
        // re-resolves the target and re-runs the focus-secured plan
        // instead of replaying a key sequence that a layout change
        // breaks.
        {
            // Beta-audit P0.3: intent linkage rides the ticket-verified
            // sink — the record belongs to the run that authorized the
            // execution, never to whichever run happens to be current.
            let sink = sess
                .evidence_sink()
                .expect("authorized dispatch installs the evidence sink");
            let (sid, gen) = (sess.id.clone(), sess.generation);
            sink.record_event(
                &sid,
                &json!({
                    "intent": intent_id,
                    "kind": "intent_executed",
                    "target": plan_json["control"],
                    "verb": plan.verb.name(),
                    "risk": plan.risk.name(),
                    "steps": executed,
                })
                .to_string(),
            );
            sink.record_scenario_intent(
                &sid,
                gen,
                json!({
                    "target": p.target,
                    "verb": intent_verb_json,
                    "sensitive": sensitive,
                }),
            );
            sink.record_scenario_wait(
                &sid,
                gen,
                json!({ "condition": "screen_stable", "note": format!("intent {intent_id} completed") }),
            );
        }
        ok(json!({
            "mode": "executed",
            "intent_id": intent_id,
            "control": plan_json["control"],
            "verb": plan.verb.name(),
            "risk": plan.risk.name(),
            "steps": executed,
        }))
    })
    .await
    .unwrap_or_else(|e| e)
}

/// JSON shape of a plan: the resolved control plus every step in execution
/// order, so the agent sees exactly what will happen before it happens.
fn plan_to_json(plan: &crate::intent::IntentPlan) -> serde_json::Value {
    let control = &plan.control;
    let steps: Vec<serde_json::Value> = plan
        .steps
        .iter()
        .map(|s| match s {
            crate::intent::PlannedStep::MoveFocus { target_id, key } => json!({
                "step": "move_focus",
                "target": target_id,
                // The action's EXACT identity (`tab`, `shift+tab`) — name()
                // would only say "key".
                "how": key.signature(),
                "non_activating": true,
                "skipped_when": "target already focused (no hops needed)",
            }),
            crate::intent::PlannedStep::AssertFocus { target_id } => json!({
                "step": "assert_focus",
                "target": target_id,
                "on_drift": "stale_state — the payload action is refused",
            }),
            crate::intent::PlannedStep::Act(action) => json!({
                "step": "act",
                "action": serde_json::to_value(action).unwrap_or(json!(action.name())),
            }),
        })
        .collect();
    json!({
        "control": {
            "id": control.id,
            "label": control.label,
            "kind": format!("{:?}", control.kind).to_lowercase(),
            "focusable": control.focusable,
            "focused": control.focused,
        },
        "steps": steps,
    })
}
