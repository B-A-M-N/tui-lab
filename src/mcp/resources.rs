//! The `tui://` resource resolver (god-object round 2, G5).
//!
//! Moves OUT of `tools.rs`: URI resolution is a read surface of its own
//! (findings feed, run evidence, session views), not server plumbing.
//! `TuiLabServer` keeps a one-line delegate; everything that decides HOW
//! a `tui://` URI becomes text lives here.
//!
//! - [`Resources::resolve`]: the four URI families (findings, findings/
//!   `<id>`, runs/..., sessions/...) with honest `resource_not_found`
//!   errors that name what WAS accepted (Wave G item 72).
//! - [`resolve_run_scoped`]: run-scoped evidence (`scenarios`,
//!   `transactions`), live-borrowed or restored read-only from disk.
//! - [`run_scoped_payload`]: the payload renderer shared by both.
//! - [`running_id`]: the manifest-authoritative run id of a persisted
//!   run directory (review P0.2 resume checks).
//!
//! Field access stays via the server's `pub(crate)` handles — the same
//! short-lock discipline as before (never held across an `.await`;
//! the session view dispatch happens ON the actor thread).

/// Which named view of a session a `tui://sessions/<id>/<view>` resource
/// resolves to. `TerminalProfile` is observationally pure — it never forces a
/// screen settle, unlike the screen-backed views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionView {
    TerminalProfile,
    Semantic,
    Screen,
}

/// Read the declared run id out of a persisted run directory (review P0.2).
/// Used to decide which live sessions legitimately belong to the run being
/// resumed. The manifest is authoritative; a directory without one cannot
/// be a valid resume target.
pub(crate) fn running_id(run_dir: &std::path::Path) -> String {
    crate::run::manifest::load(run_dir)
        .map(|m| m.run_id)
        .unwrap_or_default()
}

/// The payload for a run-scoped evidence URI, rendered against whichever
/// run context the caller resolved (live-borrowed or restored). `Err` is
/// the honest not-found message naming what would have been accepted.
pub(crate) fn run_scoped_payload(
    run: &crate::run::RunContext,
    run_id: &str,
    kind: &str,
    key: Option<&str>,
) -> Result<String, String> {
    match (kind, key) {
        // The collection listing (no key): ids + shapes, so a client can
        // address a specific one next.
        ("scenarios", None) => {
            let ids = run.list_saved_scenarios().map_err(|e| e.to_string())?;
            let items: Vec<serde_json::Value> = ids
                .iter()
                .filter_map(|k| run.load_scenario(k).ok())
                .map(|sc| {
                    serde_json::json!({
                        "id": sc.id, "name": sc.name,
                        "steps": sc.steps.len(),
                        "uri": format!("tui://runs/{run_id}/scenarios/{}", sc.id),
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "run": run_id, "scenarios": items, "count": items.len(),
            }))
            .unwrap_or_default())
        }
        // One scenario by id (or unambiguous name — load_scenario's own
        // resolution order, shared with tui_scenario action=run).
        ("scenarios", Some(key)) => {
            let sc = run.load_scenario(key).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&sc).unwrap_or_default())
        }
        // The ledger listing (no key): bounded to the retained window.
        ("transactions", None) => {
            let txs = run.transactions();
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "run": run_id,
                "retained": txs.len(),
                "lifetime": run.transaction_total(),
                "note": "the ledger is a bounded window; the manifest names any evicted head (history_complete=false)",
                "transactions": txs,
            }))
            .unwrap_or_default())
        }
        // One transaction by ledger seq.
        // First-class causal timeline: dispatch provenance + event anchors
        // + frame references + render citations joined into one artifact.
        ("timeline", None) => {
            let txs = run.transactions();
            let items: Vec<serde_json::Value> = txs
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "seq": t.seq,
                        "at": t.at,
                        "generation": t.generation,
                        "session": t.session,
                        "action": t.action,
                        "origin": t.origin,
                        "dispatch": t.dispatch,
                        "settle": t.settle,
                        "settle_reason": t.settle_reason,
                        "before_structure": t.before_structure,
                        "after_structure": t.after_structure,
                        "before_frame_id": t.before_frame_id,
                        "after_frame_id": t.after_frame_id,
                        "event_seq_before": t.event_seq_before,
                        "event_seq_after": t.event_seq_after,
                        "elapsed_ms": t.elapsed_ms,
                        "send_ms": t.send_ms,
                        "settle_ms": t.settle_ms,
                        "render": t.render,
                        "uri": format!("tui://runs/{run_id}/timeline/{}", t.seq),
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "run": run_id,
                "retained": txs.len(),
                "lifetime": run.transaction_total(),
                "note": "causal timeline over the retained transaction window; per-entry URIs address one transaction's joined evidence",
                "timeline": items,
            }))
            .unwrap_or_default())
        }
        // One joined timeline entry. This is the primary debugging artifact
        // for "I pressed X and something weird happened": no manual join
        // across ledger + frame ids + event anchors + render citations.
        ("timeline", Some(key)) => {
            let seq: u64 = key.parse().map_err(|_| {
                format!(
                    "timeline key '{key}' is not a transaction ledger seq (integer); tui://runs/{run_id}/timeline lists the retained window"
                )
            })?;
            let tx = run.transactions().iter().find(|t| t.seq == seq).ok_or_else(
                || {
                    format!(
                        "no timeline entry with seq {seq} in run '{run_id}' (retained window: {} records; the ledger may have evicted old records)",
                        run.transactions().len()
                    )
                },
            )?;
            let before_uri = tx.before_frame_id.map(|id| format!("tui://runs/{run_id}/frames/{id}"));
            let after_uri = tx.after_frame_id.map(|id| format!("tui://runs/{run_id}/frames/{id}"));
            let payload = serde_json::json!({
                "run": run_id,
                "transaction": tx,
                "references": {
                    "before_frame": before_uri,
                    "after_frame": after_uri,
                    "events": {
                        "before_seq": tx.event_seq_before,
                        "after_seq": tx.event_seq_after,
                    },
                    "render_range": tx.render.as_ref().map(|r| serde_json::json!({
                        "start": r.range_start,
                        "end": r.range_end,
                        "complete": r.complete,
                    })),
                },
                "note": "event_seq_before/after anchor the terminal event window; frame references are present when evidence committed through the sink",
            });
            Ok(serde_json::to_string_pretty(&payload).unwrap_or_default())
        }
        ("transactions", Some(key)) => {
            let seq: u64 = key.parse().map_err(|_| {
                format!(
                    "transaction key '{key}' is not a ledger seq (integer); tui://runs/{run_id}/transactions lists the retained window"
                )
            })?;
            let tx = run.transactions().iter().find(|t| t.seq == seq).ok_or_else(
                || {
                    format!(
                        "no transaction with seq {seq} in run '{run_id}' (retained window: {} records; the ledger may have evicted old records)",
                        run.transactions().len()
                    )
                },
            )?;
            Ok(serde_json::to_string_pretty(tx).unwrap_or_default())
        }
        (other, _) => Err(format!(
            "unknown run-scoped resource '{other}' under run '{run_id}' (expected 'timeline', 'scenarios', or 'transactions', each optionally followed by an id/seq)"
        )),
    }
}

/// Resource resolution against the server's live run + session pool.
/// Free functions rather than inherent methods so `tools.rs` stays the
/// thin façade and this module owns the whole read surface.
pub(crate) mod resolve {
    use super::{run_scoped_payload, SessionView};
    use crate::mcp::tools::TuiLabServer;

    /// Resolve a `tui://` resource URI to its text content (Wave G item
    /// 72). Unknown schemes/ids are honest `resource_not_found` errors that
    /// name what WAS accepted, so a stale id can be self-corrected.
    pub(crate) async fn resource(
        s: &TuiLabServer,
        uri: &str,
    ) -> Result<String, rmcp::model::ErrorData> {
        use crate::semantic;
        let not_found = |msg: String| rmcp::model::ErrorData::resource_not_found(msg, None);
        // Findings: the feed and (review P1: evidence-addressability) one
        // finding by instance id, rendered like tui_explain — evidence
        // refs joined to their sources, capabilities conditioning applied.
        if uri == "tui://findings" {
            let run = s.run.lock().unwrap();
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "run": run.id(),
                "findings": run.findings(),
                "count": run.findings().len(),
            }))
            .unwrap_or_default());
        }
        if let Some(fid) = uri.strip_prefix("tui://findings/") {
            let fid = fid.trim_end_matches('/');
            let run = s.run.lock().unwrap();
            let finding = run.findings().iter().find(|f| f.id == fid).ok_or_else(|| {
                not_found(format!(
                    "no finding '{fid}' in run '{}' (tui://findings lists the {} available)",
                    run.id(),
                    run.findings().len()
                ))
            })?;
            // Same explanation shape `tui_explain` renders, minus the live
            // session conditioning (a resource read stays observationally
            // pure — it never touches a session actor).
            let joined = run.join_source_refs_if_known(finding);
            let explanation = crate::terminal::explain::explain_finding(&joined, None, None);
            return Ok(serde_json::to_string_pretty(&explanation).unwrap_or_default());
        }
        if let Some(rest) = uri.strip_prefix("tui://runs/") {
            let rest = rest.trim_end_matches('/');
            if rest.is_empty() {
                return Err(not_found("empty run id".to_string()));
            }
            // Run-scoped evidence: scenarios and ledger transactions
            // (review P1: citable evidence must be addressable, not just
            // inlined into run status). Match these BEFORE the bare run
            // id so `<run>/scenarios/<sid>` never falls through to it.
            if let Some((run_id, sub)) = rest.split_once('/') {
                let (kind, key) = match sub.split_once('/') {
                    Some((k, key)) => (k, Some(key.trim_end_matches('/'))),
                    None => (sub, None),
                };
                return run_scoped(s, run_id, kind, key, not_found);
            }
            // The live run first (it carries live session state)…
            {
                let run = s.run.lock().unwrap();
                if rest == run.id() {
                    let sessions = s.sessions.list();
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
            let mut bases = s.run.lock().unwrap().browser_bases();
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
            let live_id = s.run.lock().unwrap().id().to_string();
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
            let snapshot = s
                .sessions
                .with_session(Some(&selector), move |sess| {
                    let selector = in_job;
                    if let SessionView::TerminalProfile = session_view {
                        // Evidence-backed capability report, no screen settle.
                        let profile = sess.terminal_profile();
                        return Some(serde_json::to_string_pretty(&profile).unwrap_or_default());
                    }
                    // Passive resource read: peek CURRENT state without a
                    // settle cycle. A cached committed frame is stale by
                    // definition when the target changes asynchronously
                    // (beta audit item 32); `peek_fresh` pumps bytes/native
                    // facts without the ordinary quiet wait. If the pump
                    // fails, the last committed frame remains the best
                    // available snapshot.
                    let screen = match sess.peek_fresh() {
                        Ok(f) => f.frame,
                        Err(_) => match sess.last() {
                            Some(f) => f.clone(),
                            None => sess.observe(40).ok()?,
                        },
                    };
                    if matches!(session_view, SessionView::Semantic) {
                        // Fused truth: the resource serves the SAME analysis
                        // observe modes see — cached detection + native
                        // overlay — never an inference-only view.
                        match sess.fused_frame() {
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
             tui://runs/{{run_id}}/scenarios/{{scenario_id}}, \
             tui://runs/{{run_id}}/transactions/{{seq}}, \
             tui://sessions/{{session_id}}/semantic, tui://sessions/{{session_id}}/screen, \
             tui://findings, tui://findings/{{finding_id}})"
        )))
    }

    /// Run-scoped evidence read: `tui://runs/<id>/scenarios[/<key>]` and
    /// `tui://runs/<id>/transactions[/<seq>]` (review P1: evidence-
    /// addressability — citable evidence gets its own URI, live or
    /// restored read-only from disk). `kind`/`key` are the path segments
    /// after the run id.
    pub(crate) fn run_scoped(
        s: &TuiLabServer,
        run_id: &str,
        kind: &str,
        key: Option<&str>,
        not_found: impl Fn(String) -> rmcp::model::ErrorData,
    ) -> Result<String, rmcp::model::ErrorData> {
        // Resolve the run: live first (borrowed, short lock — this fn is
        // sync and never awaits under it), then any persisted one restored
        // read-only from disk.
        let live_is_target = s.run.lock().unwrap().id() == run_id;
        let rendered = if live_is_target {
            let run = s.run.lock().unwrap();
            run_scoped_payload(&run, run_id, kind, key)
        } else {
            let mut bases = s.run.lock().unwrap().browser_bases();
            if let Ok(cwd) = std::env::current_dir() {
                if !bases.contains(&cwd) {
                    bases.push(cwd);
                }
            }
            let mut restored: Option<crate::run::RunContext> = None;
            for base in &bases {
                if let Some(dir) = crate::run::RunContext::resolve_run_dir(base, run_id) {
                    restored = Some(
                        crate::run::RunContext::restore(&dir)
                            .map_err(|e| not_found(format!("run '{run_id}' unreadable: {e}")))?,
                    );
                    break;
                }
            }
            let run = restored.ok_or_else(|| {
                not_found(format!(
                    "no run '{run_id}' in this server or under its runs roots"
                ))
            })?;
            run_scoped_payload(&run, run_id, kind, key)
        };
        match rendered {
            Ok(text) => Ok(text),
            Err(msg) => Err(not_found(msg)),
        }
    }
}
