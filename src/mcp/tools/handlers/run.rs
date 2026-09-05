//! tui_run: run lifecycle, persistence, diagnosis packets, registry context.

use super::super::running_id;
use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_run` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_run(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiRunParams>,
) -> rmcp::model::CallToolResult {
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
        // Review P0.1/2 + audit P0-6: begin a fresh ephemeral run. A
        // PERSISTENT current run is flushed BEFORE replacement and a
        // flush failure aborts the swap — `new` must never silently
        // destroy unflushed evidence. An ephemeral run has nothing on
        // disk to lose (held artifacts move nowhere; the response says
        // so), so it swaps directly.
        RA::New => {
            let mut old = {
                let mut guard = s.run.lock().unwrap();
                std::mem::replace(&mut *guard, crate::run::RunContext::ephemeral())
            };
            let flush_result = match old.run_dir() {
                Some(_) => old.flush(),
                None => Ok(()),
            };
            match flush_result {
                Ok(()) => {
                    let new_id = s.run.lock().unwrap().id.clone();
                    ok(json!({
                        "new_run_id": new_id,
                        "previous_run": old.id,
                        "previous_flushed": old.run_dir().is_some(),
                        "mode": "ephemeral",
                        "note": "fresh ephemeral run begun; sessions from the previous run were not touched and now belong to a foreign run",
                    }))
                }
                Err(e) => {
                    // Swap back: the old run stays live when its flush
                    // fails — evidence is never dropped on the floor.
                    let mut guard = s.run.lock().unwrap();
                    *guard = old;
                    err(
                        ErrorCategory::InternalError,
                        format!("run flush failed before starting a new run; the current run is UNCHANGED: {e}"),
                    )
                }
            }
        }
        RA::Status => {
            let sessions = s.sessions.list();
            ok(s.run.lock().unwrap().status(
                sessions
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ))
        }
        RA::Persist => {
            let mut run = s.run.lock().unwrap();
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
            // Audit P0-9: this run's sessions, not every session in the
            // process. Previous-run sessions may legitimately still be
            // alive after `tui_run new` (they are foreign); closing run B
            // must neither absorb their events nor kill them.
            let run_id = s.run.lock().unwrap().id.clone();
            let owned: Vec<String> = {
                let owners = s.session_owners.lock().unwrap();
                s.sessions
                    .list()
                    .into_iter()
                    .filter(|sid| owners.get(sid).map(|o| o.as_str()) == Some(run_id.as_str()))
                    .collect()
            };
            let foreign: Vec<String> = {
                let owners = s.session_owners.lock().unwrap();
                s.sessions
                    .list()
                    .into_iter()
                    .filter(|sid| owners.get(sid).map(|o| o.as_str()) != Some(run_id.as_str()))
                    .collect()
            };
            // Drain terminal-event queues of OWNED sessions into the run
            // (Wave B item 14) before the flush so the event logs land in
            // the artifacts. Wave G item 73: each drain is one actor call
            // — no guard is held across the loop, so there is no
            // re-entrancy dance and no close-path deadlock to avoid.
            for sid in &owned {
                if let Ok(events) = s.sessions.drain_events(sid).await {
                    let _ = s.run.lock().unwrap().hold_events(sid, events);
                }
            }
            let already;
            let summary;
            let kill;
            let result;
            {
                let mut run = s.run.lock().unwrap();
                already = run.is_closed();
                summary = run.status(
                    owned
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
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
            // OWNED sessions survive close unless explicitly requested;
            // foreign sessions are NEVER touched by another run's close.
            // Finding 4/13: a LEASED session is never killed by close
            // either — killing the process under a human driving it is the
            // exact hazard the lease exists to prevent. Leased sessions are
            // skipped and reported (`leased_not_killed`) with holder and
            // remaining TTL; they can still be stopped explicitly via
            // tui_session stop after the lease is released or expired.
            let (mut stopped, leased_not_killed) = if kill {
                let mut out = Vec::new();
                let mut leased = Vec::new();
                for sid in &owned {
                    // Lease check via the POOL, not with_sess: this runs
                    // after run.close(), and with_sess refuses a closed run
                    // — which would silently flatten to "no lease" and kill
                    // the leased session. Reading the lease is bookkeeping,
                    // not driving.
                    let live_lease = s
                        .sessions
                        .with_session(Some(sid), |sess| sess.driving_blocked())
                        .await
                        .ok()
                        .flatten();
                    if let Some(lease) = live_lease {
                        leased.push(serde_json::json!({
                            "session": sid,
                            "holder": lease.holder,
                            "remaining_ms": lease.remaining_ms(),
                        }));
                        continue;
                    }
                    if s.sessions.stop(sid).await.is_ok() {
                        s.session_owners.lock().unwrap().remove(sid);
                        out.push(sid.clone());
                    }
                }
                (out, leased)
            } else {
                (Vec::new(), Vec::new())
            };
            let _ = &mut stopped;
            ok(json!({
                "closed": true,
                "already_closed": already,
                "owned_sessions": owned,
                "foreign_live_sessions": foreign,
                "sessions_stopped": stopped,
                "leased_not_killed": leased_not_killed,
                "final": summary,
            }))
        }
        // Wave G item 68: the capability registry as JSON — the
        // machine-readable answer to "what can this server do".
        RA::Context => ok(json!({
            "registry": crate::mcp::registry::to_json(),
            // Review follow-up (coverage honesty): say what the native
            // coverage path is worth on its own, so an agent without the
            // optional tuicov executable knows the ledger is not a stub.
            "coverage": {
                "provider": "native-events",
                "mode": "continuous",
                "description": "coverage events from cooperative apps accumulate continuously into the run ledger (tui_coverage action=summary/ledger/delta) — no executable required",
                "tuicov": {
                    "available": crate::coverage::tuicov::is_available(),
                    "role": "optional point-in-time snapshot correlation; absent tuicov degrades only that view, never the ledger",
                },
            },
        })),
        // Wave G item 75: enumerate persisted runs under a resolved
        // root (explicit root, else the primary session's cwd; neither
        // present is invalid_request — the same no-guessing rule persist
        // follows). Corrupt entries are named in place, never dropped.
        RA::List => {
            let base: String = match p.root.clone() {
                    Some(r) => r,
                    None => match s.run.lock().unwrap().primary_session_cwd() {
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
                Ok(entries) => {
                    // Audit P0-45: partition ONCE. The old code drained
                    // into `runs` first, so `skipped` was always empty —
                    // corrupt entries silently vanished. A directory with
                    // no run.json lands here with its `skipped` reason;
                    // keep it — corruption is evidence.
                    let (runs, skipped): (Vec<_>, Vec<_>) =
                        entries.into_iter().partition(|e| e.get("run_id").is_some());
                    ok(json!({
                        "base": base,
                        "runs": runs,
                        "skipped": skipped,
                        "current_run": s.run.lock().unwrap().id.clone(),
                    }))
                }
                Err(e) => err(ErrorCategory::InvalidRequest, e.to_string()),
            }
        }
        // Wave G item 74 + audit P0-7/P0-8: restore a persisted run as
        // the live run. ORDER MATTERS: the target is fully restored into
        // a temporary RunContext FIRST (a malformed target is refused
        // before anything is disturbed), then the live run is flushed
        // (audit P0-7: the old comment claimed this flush; it never
        // happened), and only THEN are foreign sessions detached —
        // destructive cleanup is the last pre-commit stage, never
        // validation. Sessions are left untouched otherwise (they belong
        // to the old run; the restored run starts with none — the
        // manifest's launch specs are on disk for re-creation).
        RA::Resume => {
            // Resolve the target directory: explicit run_dir wins, else
            // run_id under the resolved base.
            let run_dir: std::path::PathBuf = if let Some(d) = p.run_dir.clone() {
                std::path::PathBuf::from(d)
            } else if let Some(rid) = p.run_id.clone() {
                let base: String = match p.root.clone() {
                        Some(r) => r,
                        None => match s.run.lock().unwrap().primary_session_cwd() {
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
            // 1. PROVE the target is restorable BEFORE anything destructive.
            let mut restored = match crate::run::RunContext::restore(&run_dir) {
                Ok(mut r) => {
                    // Resume is the designated re-open operation: a run
                    // restored while `closed` becomes live again so the
                    // resumed run can accept driving and new evidence
                    // (review P0.1). Its own driving-refusal message says
                    // "resume it ... before driving", so resume must do
                    // exactly that — otherwise a persisted run could never
                    // be driven again.
                    r.reopen();
                    r
                }
                Err(e) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!("cannot restore {}: {e}", run_dir.to_string_lossy()),
                    )
                }
            };
            // Review P0.2 provenance guard: sessions launched under a
            // DIFFERENT run must not survive into the resumed run — their
            // future traffic would land in the wrong evidence bundle. The
            // default refuses; `detach_existing_sessions=true` stops them
            // later, after the flush. Sessions owned by the TARGET run
            // carry over (they already belong to the run being resumed).
            let target_run = running_id(&run_dir);
            let foreign: Vec<String> = {
                let owners = s.session_owners.lock().unwrap();
                s.sessions
                    .list()
                    .into_iter()
                    .filter(|sid| {
                        match owners.get(sid) {
                            // Owned by the run we're resuming: fine.
                            Some(o) => o != &target_run,
                            // Unowned sessions: treat as foreign (must be
                            // re-launched under the resumed run).
                            None => true,
                        }
                    })
                    .collect()
            };
            if !foreign.is_empty() && !p.detach_existing_sessions.unwrap_or(false) {
                return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "resume refused: {} live session(s) do not belong to target run {} ({:?}); stop them or pass detach_existing_sessions=true",
                            foreign.len(),
                            target_run,
                            foreign
                        ),
                    );
            }
            // 2. FLUSH the live run (audit P0-7) — an ephemeral run with
            // nothing durable flushes trivially. On failure the current
            // run stays live and the restore is abandoned (the restored
            // context is dropped; nothing was mutated).
            let flush = {
                let mut guard = s.run.lock().unwrap();
                guard.flush()
            };
            if let Err(e) = flush {
                return err(
                    ErrorCategory::InternalError,
                    format!("current run flush failed before resume; resume ABORTED and the current run is unchanged: {e}"),
                );
            }
            // 3. DETACH foreign sessions (audit P0-8: only after the
            // target proved restorable and the live run's evidence is
            // safely persisted).
            for sid in &foreign {
                let _ = s.sessions.stop(sid).await;
                s.session_owners.lock().unwrap().remove(sid);
            }
            // 4. ATOMIC swap under a short lock (never across an await).
            let manifest_like = {
                let mut guard = s.run.lock().unwrap();
                let prev_id = guard.id.clone();
                let restored_id = restored.id.clone();
                let restored_dir = restored
                    .run_dir()
                    .map(|d| d.to_string_lossy().to_string())
                    .unwrap_or_default();
                let summary = restored.status(Vec::new());
                // Audit P1-46: the resume response names what the restore
                // could not bring back — a damaged run is usable but the
                // agent must see the evidence is incomplete.
                let restore_health = restored.restore_health();
                *guard = std::mem::replace(&mut restored, crate::run::RunContext::ephemeral());
                json!({
                    "resumed": true,
                    "run_id": restored_id,
                    "previous_run": prev_id,
                    "flushed_previous": true,
                    "artifact_root": restored_dir,
                    "restore": restore_health,
                    "status": summary,
                    "note": "sessions are not restored; re-create them with tui_session start and the run will correlate them. history_complete=false runs replay only from their declared first_available_seq",
                })
            };
            ok(manifest_like)
        }
        // ── diagnose/repair: DiagnosticContexts for every finding ──
        // Review §2: evidence contexts for investigation, not fix
        // prescriptions. `repair` is an accepted alias (same arm); the
        // response's `contract` field names the recontracted meaning so
        // pre-beta callers see the change.
        RA::Diagnose | RA::Repair => {
            let contract_note: Option<String> = if matches!(run_action, RA::Repair) {
                Some("'repair' is now an alias: this surface assembles diagnostic evidence (provenance-tiered loci, verification plans, next observations) — it does not prescribe or make edits".to_string())
            } else {
                None
            };
            let (contexts, skipped) = {
                let run = s.run.lock().unwrap();
                run.diagnostic_contexts()
            };
            if contexts.is_empty() && skipped == 0 {
                return ok(json!({
                    "contract": "diagnostic",
                    "contexts": [],
                    "skipped": 0,
                    "note": "no findings recorded in this run — run an audit first (tui_audit action=run)",
                }));
            }
            ok(json!({
                "contract": "diagnostic",
                "alias_for": if matches!(run_action, RA::Repair) { json!("diagnose") } else { serde_json::Value::Null },
                "contexts": contexts,
                "skipped": skipped,
                "note": contract_note.unwrap_or_else(|| if skipped > 0 {
                    format!("{skipped} finding(s) could not form a context (no evidence) and were skipped")
                } else {
                    "each context: the finding, provenance-tiered source loci, a verification plan (targeted checks; replay only with a reproduction), and observation-shaped next steps".to_string()
                }),
            }))
        }
        // ── bundle (Wave 5 item 46): the "did the change hold?" packet ──
        // One finding's diagnostic context joined with the labeled
        // before/after diff: what the finding was, how a change can be
        // verified, what the fresh audit pass says about THIS finding,
        // and — the navigation-regression guard — every OTHER finding
        // that changed between the same two passes.
        RA::Bundle => {
            let Some(finding_id) = p.finding_id.clone() else {
                return err(
                    ErrorCategory::InvalidRequest,
                    "bundle requires 'finding_id' (from tui://findings or the audit response)",
                );
            };
            let compare_label = p.compare_to.clone().unwrap_or_else(|| "baseline".into());
            let run = s.run.lock().unwrap();
            let Some(finding) = run.findings().iter().find(|f| f.id == finding_id) else {
                let labels = run.finding_baseline_labels();
                return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "unknown finding id '{finding_id}' in run '{}'. Record audits with label= to build baselines (stored: {})",
                            run.id,
                            if labels.is_empty() { "none".to_string() } else { labels.join(", ") }
                        ),
                    );
            };
            // The context for THIS finding (pure join; contexts read run
            // state and never mutate it).
            let (contexts, _skipped) = run.diagnostic_contexts();
            let packet = contexts.into_iter().find(|c| c.finding.id == finding_id);
            // The before/after verdicts from the labeled baseline.
            let baseline = run.finding_baseline(&compare_label);
            let (verdicts, before, regressions): (
                serde_json::Value,
                serde_json::Value,
                Vec<serde_json::Value>,
            ) = match baseline {
                None => (serde_json::Value::Null, serde_json::Value::Null, Vec::new()),
                Some(base) => {
                    // REGRESSED reachable (review P1 item 12): a finding
                    // seen in an earlier pass but absent from this
                    // baseline is a regression when it reappears.
                    let resolved = crate::audit::compare::Resolved(
                        run.resolved_finding_fingerprints(&compare_label),
                    );
                    let compared = crate::audit::compare::compare_with_resolved(
                        base,
                        run.findings(),
                        &resolved,
                    );
                    let this = compared
                        .iter()
                        .find(|c| c.finding.id == finding_id)
                        .map(|c| {
                            json!({
                                "fingerprint": c.fingerprint,
                                "verdict": c.verdict,
                            })
                        })
                        .unwrap_or(json!({
                            "fingerprint": crate::audit::compare::fingerprint(finding),
                            "verdict": "fixed",
                            "note": "the bundled finding no longer appears in the current set",
                        }));
                    // The regression guard: every OTHER finding whose
                    // verdict moved the wrong way between the passes.
                    let others: Vec<serde_json::Value> = compared
                        .iter()
                        .filter(|c| c.finding.id != finding_id)
                        .filter(|c| c.verdict == "new" || c.verdict == "regressed")
                        .map(|c| {
                            json!({
                                "id": c.finding.id,
                                "category": c.finding.category,
                                "summary": c.finding.summary,
                                "verdict": c.verdict,
                            })
                        })
                        .collect();
                    let before_f = base.iter().find(|b| b.id == finding_id).map(|b| {
                        json!({
                            "id": b.id,
                            "summary": b.summary,
                            "severity": b.severity,
                        })
                    });
                    (this, before_f.unwrap_or(serde_json::Value::Null), others)
                }
            };
            ok(json!({
                "finding_id": finding_id,
                "rule_id": finding.rule_id,
                "summary": finding.summary,
                "context": packet,
                "before": before,
                "after": verdicts,
                "baseline": compare_label,
                "baseline_available": baseline.is_some(),
                "side_effects": {
                    "new_or_regressed_elsewhere": regressions,
                    "count": regressions.len(),
                    "note": "navigation-regression guard: OTHER findings that appeared or worsened between the same two passes",
                },
                "note": if baseline.is_some() {
                    "bundle = diagnostic context + this finding's before/after verdict + any side effects in the same diff"
                } else {
                    "no baseline labeled '{compare_label}' — run tui_audit label=baseline before the change and compare_to=baseline after"
                },
            }))
        }
    }
}
