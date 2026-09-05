//! tui_coverage: native coverage ledger views.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_with_details, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_coverage` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_coverage(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiCoverageParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::CoverageAction as CV;
    let action = p
        .action
        .clone()
        .unwrap_or(crate::mcp::params::Known::Known(CV::Summary));
    // The run-ledger views (summary/collect/delta/ledger) are answered
    // here against the REAL accumulated evidence; un-covered is an honest
    // unsupported until a denominator source exists (review P0.7). detect
    // and the optional tuicov snapshot delegate to the provider module.
    match action.known() {
        Some(CV::Ledger) | Some(CV::Summary) => {
            let run = s.run.lock().unwrap();
            let entries: Vec<serde_json::Value> = run
                .coverage_ledger()
                .iter()
                .map(|(target, e)| {
                    json!({
                        "target": target,
                        "hits": e.hits,
                        "sessions": e.sessions,
                        "first_seq": e.first_seq,
                        "last_seq": e.last_seq,
                        "first_seen": e.first_seen,
                        "last_seen": e.last_seen,
                        "source_refs": e.source_refs,
                    })
                })
                .collect();
            let total_hits: u64 = run.coverage_ledger().values().map(|e| e.hits).sum();
            let sessions_seen = run.coverage_seq() > 0;
            ok(json!({
                "ledger": entries,
                "targets": entries.len(),
                "total_hits": total_hits,
                "provider": "native-events",
                "mode": "continuous",
                "probe": json!({
                    "tuicov": crate::coverage::tuicov::is_available(),
                    "note": "tuicov is an optional snapshot executable; the run ledger is the source of truth for accumulated evidence",
                }),
                "note": if entries.is_empty() {
                    Some("no native coverage events yet; cooperative apps send coverage events over TUI_LAB_SEMANTIC".to_string())
                } else if !sessions_seen {
                    Some("run restored from a pre-cursor manifest; counts are intact but sequence order reflects live events since reopen".to_string())
                } else { None },
            }))
        }
        Some(CV::Collect) => {
            // Collection is CONTINUOUS — there is nothing to trigger.
            // This honestly reports that, plus what has accumulated, and
            // never pretends a silent no-op "collected" (review P0.7).
            let run = s.run.lock().unwrap();
            let targets = run.coverage_ledger().len();
            let total_hits: u64 = run.coverage_ledger().values().map(|e| e.hits).sum();
            ok(json!({
                "mode": "continuous",
                "provider": "native-events",
                "note": "native coverage is collected continuously into the run ledger as events arrive; no explicit collect needed",
                "targets_collected_so_far": targets,
                "total_hits_so_far": total_hits,
            }))
        }
        Some(CV::Delta) => {
            // Real delta against the accumulated ledger (review P0.7).
            // Review §13: the cursor belongs to the CALLER. With
            // `since_seq`, the read is stateless — two agents each get the
            // full window after their own cursor, and neither consumes the
            // other's. Without it, the run's own cursor is used and
            // advanced (the historical single-consumer contract).
            let mut run = s.run.lock().unwrap();
            let (cur, caller_owned) = match p.since_seq {
                Some(seq) => (seq, true),
                None => (run.coverage_delta_cursor(), false),
            };
            let mut new_targets = Vec::new();
            let mut hits_since: u64 = 0;
            for (target, e) in run.coverage_ledger() {
                if e.first_seq > cur {
                    new_targets.push(json!({
                        "target": target,
                        "hits": e.hits,
                        "first_seq": e.first_seq,
                    }));
                    hits_since += e.hits;
                }
            }
            new_targets.sort_by(|a, b| a["first_seq"].as_u64().cmp(&b["first_seq"].as_u64()));
            let new_cursor = run.coverage_seq();
            if !caller_owned {
                run.set_coverage_delta_cursor(new_cursor);
            }
            ok(json!({
                "new_targets": new_targets,
                "new_target_count": new_targets.len(),
                // Audit finding 30 (honest naming): this is the hit count ON
                // THE NEW TARGETS, not a delta of all hits — hits on
                // already-known targets after the cursor are not counted
                // (no per-hit journal exists to know them).
                "hits_on_new_targets": hits_since,
                "cursor_was": cur,
                "cursor_now": new_cursor,
                // Repeat this call with since_seq=cursor_now to see only
                // what lands after this response.
                "cursor_mode": if caller_owned { "caller" } else { "run" },
                "exhausted": new_targets.is_empty(),
            }))
        }
        Some(CV::Uncovered) => {
            // HONEST unsupported: without a denominator (what the app
            // COULD cover) we cannot say what is uncovered. Inventing one
            // would be theater, so this is an explicit refusal (review
            // P0.7). Native coverage only knows what was hit. The working
            // views are named in `details.alternatives` (review §14).
            err_with_details(
                    ErrorCategory::Unsupported,
                    "coverage action 'uncovered' is unsupported: tui-lab has no denominator of what the app *could* cover, so it reports hits honestly and refuses to fabricate a gap; use delta (what changed since the last read) instead",
                    json!({
                        "action": "uncovered",
                        "alternatives": ["summary", "ledger", "delta", "collect"],
                    }),
                )
        }
        // detect (and anything unexpected) dispatches through the
        // provider module, which knows what the tuicov executable offers.
        _ => match crate::coverage::tuicov::handle(&p) {
            Ok(s) => crate::mcp::helpers::ok_from_json(&s),
            Err(e) => err(ErrorCategory::BackendError, e.to_string()),
        },
    }
}
