//! tui_act / tui_checkpoint: canonical driving and checkpoint comparison.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_invalid_selector, lease_refused, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

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
    let run = s.run.clone();
    // The actor closure owns the session side (observe) and does the
    // checkpoint work against the run under a short lock.
    s.with_sess(p.id.as_deref(), move |sess| {
        let session_id = sess.id.clone();
        let generation = sess.generation;
        match ckpt_action {
            CA::List => {
                let run = run.lock().unwrap();
                ok(json!({ "checkpoints": run.checkpoints.list(&session_id) }))
            }
            CA::Save => {
                let (screen, sem, _, _) = match sess.observe_fused(40) {
                    Ok(t) => t,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
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
                    None => return err(ErrorCategory::InvalidRequest, "compare requires 'name'"),
                };
                let (screen, sem, _, _) = match sess.observe_fused(40) {
                    Ok(t) => t,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
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
                    None => return err(ErrorCategory::InvalidRequest, "delete requires 'name'"),
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
    s.with_sess(selector.as_deref(), move |sess| {
            // Wave G item 76: a live human control lease blocks driving.
            if let Some(refused) = lease_refused(sess) {
                return refused;
            }
            let tx = match crate::execution::execute_act_with_guard(
                sess,
                &action,
                quiet,
                quiet.saturating_add(1000),
                p.no_wait(),
                visibility,
                completion,
                // Re-review P0.9: an agent-declared expected-state guard is
                // validated atomically with the send. Drift refuses the
                // action with a structured stale_state verdict instead of
                // landing input in a changed UI.
                p.guard().map(|g| g.to_guard()).as_ref(),
            ) {
                Ok(t) => t,
                Err(e) => {
                    // A guard refusal is not a backend failure: classify it
                    // so the agent knows to re-observe, not to retry blind.
                    let msg = e.to_string();
                    if msg.starts_with("stale_state") {
                        return err(ErrorCategory::StaleState, msg);
                    }
                    return err(ErrorCategory::BackendError, msg);
                }
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
                // Frame commit pipeline (re-review item 40): both frames go
                // through the ONE commit path — id + provenance + incremental
                // frames.jsonl append for persistent runs.
                let b = run
                    .commit_frame(
                        &mut {
                            let mut f = tx.before_frame.clone();
                            f.session_id = Some(sid.clone());
                            f.generation = Some(gen);
                            f
                        },
                        Some(&sid),
                    )
                    .unwrap_or_default();
                let a = run
                    .commit_frame(
                        &mut {
                            let mut f = tx.after_frame.clone();
                            f.session_id = Some(sid.clone());
                            f.generation = Some(gen);
                            f
                        },
                        Some(&sid),
                    )
                    .unwrap_or_default();
                // Run ledger (Wave-2 item 15): the reconstructable transaction
                // record, not just a counter. The ledger projects the action
                // through the visibility policy — sensitive payloads are stored
                // as Redacted(kind, byte_len), never verbatim.
                let _ = run.record_interaction(&sid, &tx);
                // Scenario capture (see block comment above): sensitive steps
                // are recorded as ${PARAM} references with the parameter
                // declared on the scenario — never with the payload (re-review
                // P0.3). The recorded step stays replayable: a caller that
                // supplies the parameter gets an exact replay; one that
                // doesn't gets a structured `unresolved_parameter` step
                // failure instead of a corrupt scenario.
                if sensitive {
                    // The canonical payload field for the two string-payload
                    // actions; raw bytes are recorded as an opaque redacted
                    // step (no string field to reference).
                    let (payload_field, params_json) = match serde_json::to_value(&p) {
                        Ok(v) => match tx.canonical() {
                            crate::execution::CanonicalAction::Type { .. } => ("text".to_string(), v),
                            crate::execution::CanonicalAction::Paste { .. } => ("paste".to_string(), v),
                            _ => (String::new(), v),
                        },
                        Err(_) => (String::new(), json!({})),
                    };
                    if !payload_field.is_empty() {
                        let byte_len = tx.canonical().payload_len();
                        let _ = run.record_scenario_act_sensitive(
                            &sid,
                            gen,
                            params_json,
                            &payload_field,
                            crate::scenario::model::SensitiveKind::Secret,
                            byte_len,
                        );
                    } else {
                        // Raw-bytes (or exotic) sensitive action: keep the
                        // opaque redacted placeholder — structurally a valid
                        // act step is impossible without the payload, and the
                        // scenario declares it unreplayable-by-shape.
                        let _ = run.record_scenario_act(
                            &sid,
                            gen,
                            json!({
                                "action": tx.name(),
                                "sensitive": true,
                                "redacted": true,
                                "payload_bytes": tx.canonical().payload_len(),
                            }),
                        );
                    }
                } else {
                    let _ = run.record_scenario_act(
                        &sid,
                        gen,
                        serde_json::to_value(&p).unwrap_or_default(),
                    );
                }
                serde_json::json!({ "before": format!("frame:{b}"), "after": format!("frame:{a}") })
            };
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
                "render": render,
                "transition": tx.transition,
            }))
        })
        .await
        .unwrap_or_else(|e| e)
}
