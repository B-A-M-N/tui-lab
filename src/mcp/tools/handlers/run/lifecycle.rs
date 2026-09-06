//! Run lifecycle actions (god-object round 2, G5b): `new`, `close`,
//! `resume` — the three arms of `tui_run` that REPLACE or RETIRE the
//! live run context. Split out of the former single `tui_run` match;
//! each body is verbatim, only the surrounding dispatch moved.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_with_details, ok};
use crate::mcp::params::TuiRunParams;
use crate::mcp::resources::running_id;
use rmcp::serde_json::json;

/// Review P0.1/2 + audit P0-6: begin a fresh ephemeral run. A
/// PERSISTENT current run is flushed BEFORE replacement and a
/// flush failure aborts the swap — `new` must never silently
/// destroy unflushed evidence. An EPHEMERAL run with evidence,
/// though, has everything to lose: finding 33 — `new` REFUSES
/// over in-memory-only evidence unless `discard=true` names the
/// loss the caller accepted (or the caller persists first, which
/// moves the bundle to disk where `new` can flush it).
pub(crate) async fn new(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
    let (loss, loss_persistent) = {
        let guard = s.run.lock().unwrap();
        let l = guard.unsaved_evidence();
        (l, guard.run_dir().is_some())
    };
    if !loss_persistent
        && loss["lost_if_dropped"].as_u64().unwrap_or(0) > 0
        && !p.discard.unwrap_or(false)
    {
        let (run_id, lost) = {
            let g = s.run.lock().unwrap();
            (g.id().to_string(), loss["lost_if_dropped"].clone())
        };
        return err_with_details(
            ErrorCategory::InvalidRequest,
            format!(
                "tui_run new refused: current ephemeral run '{run_id}' holds {lost} evidence record(s) that exist only in memory",
            ),
            json!({
                "current_run": run_id,
                "lost_if_dropped": lost,
                "evidence": loss["evidence"],
                "hint": "run tui_run action=persist first (keeps the bundle on disk), or re-send with discard=true to abandon it deliberately",
            }),
        );
    }
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
            let new_id = s.run.lock().unwrap().id().to_string();
            ok(json!({
                "new_run_id": new_id,
                "previous_run": old.id(),
                "previous_flushed": old.run_dir().is_some(),
                "mode": "ephemeral",
                // Finding 33: name what a discard threw away, so a
                // `discard=true` caller sees the accepted loss.
                "discarded_evidence": if loss["lost_if_dropped"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
                {
                    loss
                } else {
                    serde_json::Value::Null
                },
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

/// Audit P0-9: close THIS run's sessions, not every session in the
/// process. Previous-run sessions may legitimately still be alive
/// after `tui_run new` (they are foreign); closing run B must neither
/// absorb their events nor kill them.
pub(crate) async fn close(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
    let run_id = s.run.lock().unwrap().id().to_string();
    let live = s.sessions.list();
    let owned: Vec<String> = s.session_owners.lock().unwrap().sessions_of(&live, &run_id);
    let foreign: Vec<String> = s
        .session_owners
        .lock()
        .unwrap()
        .foreign_sessions(&live, &run_id);
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
                s.session_owners.lock().unwrap().unbind(sid);
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

/// Wave G item 74 + audit P0-7/P0-8: restore a persisted run as
/// the live run. ORDER MATTERS: the target is fully restored into
/// a temporary RunContext FIRST (a malformed target is refused
/// before anything is disturbed), then the live run is flushed
/// (audit P0-7: the old comment claimed this flush; it never
/// happened), and only THEN are foreign sessions detached —
/// destructive cleanup is the last pre-commit stage, never
/// validation. Sessions are left untouched otherwise (they belong
/// to the old run; the restored run starts with none — the
/// manifest's launch specs are on disk for re-creation).
pub(crate) async fn resume(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
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
        let live = s.sessions.list();
        s.session_owners
            .lock()
            .unwrap()
            .foreign_sessions(&live, &target_run)
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
        s.session_owners.lock().unwrap().unbind(sid);
    }
    // 4. ATOMIC swap under a short lock (never across an await).
    let manifest_like = {
        let mut guard = s.run.lock().unwrap();
        let prev_id = guard.id().to_string();
        let restored_id = restored.id().to_string();
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
            // Finding 32: the reopened run is a new epoch of the
            // persisted run — records written from here on are
            // distinguishable from the original process's history.
            "resume_epoch": summary["resume_epoch"].clone(),
            "restore": restore_health,
            "status": summary,
            "note": "sessions are not restored; re-create them with tui_session start and the run will correlate them. history_complete=false runs replay only from their declared first_available_seq",
        })
    };
    ok(manifest_like)
}
