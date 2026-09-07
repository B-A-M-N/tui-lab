//! Run query + diagnosis surfaces (god-object round 2, G5b): `status`,
//! `context`, `diagnose`/`repair`, and `bundle` — the read-shaped
//! `tui_run` arms. None of them mutate the run (diagnostic contexts
//! are pure joins over run state). Split out of the former single
//! `tui_run` match; bodies are verbatim.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::{RunAction, TuiRunParams};
use rmcp::serde_json::json;

/// The live run's status snapshot, with live session ids attached.
pub(crate) fn status(s: &crate::mcp::tools::TuiLabServer) -> rmcp::model::CallToolResult {
    let sessions = s.sessions.list();
    ok(s.run.lock().unwrap().status(
        sessions
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
    ))
}

/// Wave G item 68: the capability registry as JSON — the
/// machine-readable answer to "what can this server do".
pub(crate) fn context() -> rmcp::model::CallToolResult {
    ok(json!({
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
    }))
}

/// ── diagnose/repair: DiagnosticContexts for every finding ──
/// Review §2: evidence contexts for investigation, not fix
/// prescriptions. `repair` is an accepted alias (same arm); the
/// response's `contract` field names the recontracted meaning so
/// pre-beta callers see the change.
pub(crate) fn diagnose(
    run_action: RunAction,
    s: &crate::mcp::tools::TuiLabServer,
) -> rmcp::model::CallToolResult {
    let contract_note: Option<String> = if matches!(run_action, RunAction::Repair) {
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
        "alias_for": if matches!(run_action, RunAction::Repair) { json!("diagnose") } else { serde_json::Value::Null },
        "contexts": contexts,
        "skipped": skipped,
        "note": contract_note.unwrap_or_else(|| if skipped > 0 {
            format!("{skipped} finding(s) could not form a context (no evidence) and were skipped")
        } else {
            "each context: the finding, provenance-tiered source loci, a verification plan (targeted checks; replay only with a reproduction), and observation-shaped next steps".to_string()
        }),
    }))
}

/// ── bundle (Wave 5 item 46): the "did the change hold?" packet ──
/// One finding's diagnostic context joined with the labeled
/// before/after diff: what the finding was, how a change can be
/// verified, what the fresh audit pass says about THIS finding,
/// and — the navigation-regression guard — every OTHER finding
/// that changed between the same two passes.
pub(crate) fn bundle(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
    let Some(finding_id) = p.finding_id.clone() else {
        return err(
            ErrorCategory::InvalidRequest,
            "bundle requires 'finding_id' (from tui://findings or the audit response)",
        );
    };
    let compare_label = p.compare_to.clone().unwrap_or_else(|| "baseline".into());
    let run = s.run.lock().unwrap();
    // Beta-audit P0.10: the "current set" for a did-the-fix-hold
    // comparison is the LATEST COMPLETED AUDIT PASS — a first-class
    // snapshot — never the cumulative finding ledger. The ledger keeps
    // every finding ever recorded in the run, so a defect that was in
    // the baseline, was fixed, and is absent from the newest pass would
    // still match a stale copy there and read `persisting` forever.
    //
    // The comparable gate comes FIRST: with no completed pass in this
    // run, nothing is comparable — whatever the finding id. (The id may
    // belong to a PREVIOUS run's ledger; refusing on it before the gate
    // would hide the real answer behind an unrelated error.)
    let current_set: Vec<crate::audit::Finding> = match run.latest_audit_pass() {
        Some(pass) => pass.to_vec(),
        None => {
            let finding = run.findings().iter().find(|f| f.id == finding_id);
            return ok(json!({
                "finding_id": finding_id,
                "rule_id": finding.map(|f| f.rule_id.clone()),
                "summary": finding.map(|f| f.summary.clone()),
                "baseline": compare_label,
                "baseline_available": run.finding_baseline(&compare_label).is_some(),
                "comparable": false,
                "note": "no completed audit pass recorded in this run yet — run tui_audit action=run (label=baseline before the change; compare_to=baseline after). The finding ledger is cumulative history and is deliberately NOT used as the current set.",
            }));
        }
    };
    let Some(finding) = run.findings().iter().find(|f| f.id == finding_id) else {
        let labels = run.finding_baseline_labels();
        return err(
            ErrorCategory::InvalidRequest,
            format!(
                "unknown finding id '{finding_id}' in run '{}'. Record audits with label= to build baselines (stored: {})",
                run.id(),
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
            let resolved =
                crate::audit::compare::Resolved(run.resolved_finding_fingerprints(&compare_label));
            let compared =
                crate::audit::compare::compare_with_resolved(base, &current_set, &resolved);
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
        "comparable": true,
        "current_set": "latest_audit_pass",
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
