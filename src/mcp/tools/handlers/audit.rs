//! tui_audit / tui_explain: audit orchestration and finding explanation.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_explain` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_explain(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiExplainParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    let run = s.run.clone();

    // Resolve the finding from the run ledger. Wave 5 item 45: when
    // the STORED finding carries no loci but the run's coverage
    // ledger has since learned loci for this finding's evidence
    // target (coverage events often arrive after the audit pass),
    // the join happens HERE so the explanation is source-linked even
    // when the audit ran first. A pure lookup — the stored ledger is
    // not mutated.
    let finding = {
        let run = run.lock().unwrap();
        run.findings()
            .iter()
            .find(|f| f.id == p.finding_id)
            .map(|f| run.join_source_refs_if_known(f))
    };
    let finding = match finding {
        Some(f) => f,
        None => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown finding id '{}' — not in the current run (run '{}'); \
                         re-run tui_audit or read tui://findings",
                    p.finding_id,
                    run.lock().unwrap().id,
                ),
            );
        }
    };

    // Explain it, optionally conditioned on a session's live profile.
    let finding_for_job = finding.clone();
    let selector = p.id.clone();
    let explanation = match selector {
        Some(id) => s
            .with_sess(Some(&id), move |sess| {
                let profile = sess.terminal_profile();
                crate::terminal::explain::explain_finding(&finding_for_job, None, Some(&profile))
            })
            .await
            .unwrap_or_else(|_e| {
                // Session lookup failed; explain without terminal context.
                crate::terminal::explain::explain_finding(&finding, None, None)
            }),
        None => crate::terminal::explain::explain_finding(&finding, None, None),
    };

    ok(serde_json::to_value(explanation).unwrap_or(serde_json::json!({})))
}

/// Body of `tui_audit` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_audit(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiAuditParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::AuditProfile as AP;
    let profile_sel = p
        .profile
        .clone()
        .unwrap_or(crate::mcp::params::Known::Known(AP::Full));
    let profile = match &profile_sel {
        crate::mcp::params::Known::Known(ap) => ap.engine_name().to_string(),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown audit profile '{}' (expected one of: {})",
                    o,
                    <AP as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    };
    let profile_wire = match &profile_sel {
        crate::mcp::params::Known::Known(ap) => ap.as_str().to_string(),
        _ => profile.clone(),
    };
    // Wave G item 76 (revised, review §5): the lease gate asks whether
    // the profile DRIVES (sends UI input / resizes / consumes the
    // process), not merely whether it needs a live session. One boolean
    // (the old `is_active`, now `needs_live_session`) previously conflated
    // those, so color/terminal-modes/
    // rendering/input-protocol/shell-cli/lifecycle/query_response — all
    // passive readers — wrongly refused while a human held the lease.
    // The engine's requires_exclusive_control() is the single authority
    // for the split; the MCP layer keeps no second list.
    let profile_parsed = crate::audit::orchestrator::AuditProfile::parse(&profile).ok();
    let profile_drives = profile_parsed
        .as_ref()
        .map(|p| p.requires_exclusive_control())
        .unwrap_or(false);
    // Review §6: process-consuming audits (lifecycle_exit exits and
    // relaunches the target) need their OWN authorization —
    // allow_mutation=true (which exists to permit mouse/states-style
    // drivers) must never silently authorize killing the app.
    let profile_consumes_process = profile_parsed
        .as_ref()
        .map(|p| p.risk() == crate::audit::orchestrator::MutationRisk::RestartRequired)
        .unwrap_or(false);

    // Wave 4 items 34/36/37: the mutation-safety policy. Default is
    // safe-only (observational profiles run, invasive ones are
    // withheld with an ORCH-GATED finding naming how to allow them);
    // allow_mutation=true lifts it; deep_isolation=true additionally
    // restart-replays between mutating drivers. The engine applies the
    // policy — the MCP layer only picks which one and surfaces the
    // risk class in the response.
    let policy = if p.deep_isolation.unwrap_or(false) {
        crate::audit::orchestrator::SafetyPolicy::DeepIsolation
    } else if p.allow_mutation.unwrap_or(false) {
        crate::audit::orchestrator::SafetyPolicy::AllowMutation
    } else {
        crate::audit::orchestrator::SafetyPolicy::SafeOnly
    };
    let risk = crate::audit::orchestrator::AuditProfile::parse(&profile)
        .map(|ap| ap.risk().name().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // The audit ENGINE owns the static-vs-active decision (re-review
    // P0 fix 2): `full` is the composite (static + every non-
    // process-consuming driver), and no profile is weaker than its
    // members. The MCP layer only surfaces the resulting mode. Item 49:
    // the loaded contract rides along for profile=contract.
    let selector = p.id.clone();
    let allow_process_restart = p.allow_process_restart.unwrap_or(false);
    let run = s.run.clone();
    let label = p.label.clone();
    let compare_to = p.compare_to.clone();
    s.with_sess(selector.as_deref(), move |sess| {
            if profile_consumes_process && !allow_process_restart {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "profile '{}' consumes the target process (exits and relaunches it). This requires explicit allow_process_restart=true; allow_mutation does not authorize it, and profile=full never includes it.",
                        profile
                    ),
                );
            }
            if profile_drives {
                if let Some(refused) = lease_refused(sess) {
                    return refused;
                }
            }
            let contract = run.lock().unwrap().contract().cloned();
            let report =
                match crate::audit::orchestrator::run_profile_checked(
                    sess,
                    &profile,
                    contract.as_ref(),
                    policy,
                ) {
                    Ok(r) => r,
                    Err(msg) => return err(ErrorCategory::InvalidRequest, msg),
                };

            // Findings accumulate in the run context (composition root) so a
            // later audit/coverage query can see prior evidence. Driven focus
            // edges merge into the run's persistent ID-keyed FocusGraph (Wave D
            // item 36), so multiple audits accumulate traversal proof.
            // Wave G item 67: `label` stores this pass as a named baseline;
            // `compare_to` diffs the fresh pass against a stored one —
            // FIXED / NEW / PERSISTING per fingerprint, honestly reporting
            // a missing baseline instead of fabricating an empty compare.
            let mut compare_block = serde_json::Value::Null;
            let focus_summary = {
                let mut run = run.lock().unwrap();
                run.focus_graph.merge(&report.focus_graph);
                // W2.10: attach app-declared source loci where coverage
                // evidence can name the finding's control.
                let _ = run.extend_findings_with_source_refs(report.findings.clone());
                if let Some(cmp_label) = compare_to.as_deref() {
                    match run.finding_baseline(cmp_label) {
                        Some(baseline) => {
                            // REGRESSED is reachable: a finding absent from
                            // this baseline but seen in an EARLIER pass is a
                            // regression, not a first-seen new defect (review
                            // P1 item 12).
                            let resolved = crate::audit::compare::Resolved(
                                run.resolved_finding_fingerprints(cmp_label),
                            );
                            let compared =
                                crate::audit::compare::compare_with_resolved(baseline, &report.findings, &resolved);
                            compare_block = json!({
                                "baseline": cmp_label,
                                "available": true,
                                "counts": crate::audit::compare::summary(&compared),
                                "findings": compared,
                            });
                        }
                        None => {
                            let labels = run.finding_baseline_labels();
                            compare_block = json!({
                                "baseline": cmp_label,
                                "available": false,
                                "error": format!(
                                    "no finding baseline labeled '{}' (stored: {})",
                                    cmp_label,
                                    if labels.is_empty() { "none".to_string() } else { labels.join(", ") }
                                ),
                            });
                        }
                    }
                }
                if let Some(lbl) = label.as_deref() {
                    run.record_finding_baseline(lbl, report.findings.clone());
                }
                json!({
                    "nodes": run.focus_graph.nodes.len(),
                    "edges": run.focus_graph.edges.len(),
                    "tab_cycle": run.focus_graph.tab_cycle(),
                    "reverse_tab_gaps": run.focus_graph.reverse_tab_gaps(),
                })
            };
            ok(json!({
                "profile": profile_wire,
                "mode": report.mode,
                "risk": risk,
                "policy": match policy {
                    crate::audit::orchestrator::SafetyPolicy::SafeOnly => "safe_only",
                    crate::audit::orchestrator::SafetyPolicy::AllowMutation => "allow_mutation",
                    crate::audit::orchestrator::SafetyPolicy::DeepIsolation => "deep_isolation",
                },
                // Info-split (review P1 item 9): `finding_count` has always
                // counted every row, but many are informational orchestration
                // records (ORCH-RESTART, AUDIT-METRICS, DISC-001…) — counting
                // them as "findings" makes a clean run look defect-heavy. The
                // breakdown separates defects from bookkeeping; the totals
                // stay as they were so nothing downstream changes shape.
                "finding_count": report.findings.len(),
                "findings_by_severity": {
                    "error": report.findings.iter().filter(|f| f.severity == "error").count(),
                    "warn": report.findings.iter().filter(|f| f.severity == "warn").count(),
                    "info": report.findings.iter().filter(|f| f.severity == "info").count(),
                },
                "defect_count": report
                    .findings
                    .iter()
                    .filter(|f| f.severity != "info")
                    .count(),
                "findings": report.findings,
                "focus_graph": focus_summary,
                "labeled_as": label,
                "compare": compare_block,
            }))
        })
        .await
        .unwrap_or_else(|e| e)
}
