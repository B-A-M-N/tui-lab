//! tui_contract: project-contract load/validate/compare/scaffold.

use super::super::{contract_mode_override, diff_contract_reports};
use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_contract` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_contract(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiContractParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::ContractAction as CT;
    let Some(ct_action) = (match &p.action {
        crate::mcp::params::Known::Known(a) => Some(*a),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown contract action '{}' (expected one of: {})",
                    o,
                    <CT as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    match ct_action {
        // ── load: parse + validate + remember + apply policy ──
        CT::Load => {
            let Some(path) = p.path.clone() else {
                return err(ErrorCategory::InvalidRequest, "load requires 'path'");
            };
            let contract = match crate::design::load_design_contract(std::path::Path::new(&path)) {
                Ok(c) => c,
                Err(e) => return err(ErrorCategory::InvalidRequest, e),
            };
            // Apply the contract's normalization policy to every live
            // session (item 48): subsequent structure hashes collapse the
            // declared volatile patterns.
            let policy = match contract.normalization_policy() {
                Ok(pol) => std::sync::Arc::new(pol),
                Err(e) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!("contract volatile_patterns invalid: {e}"),
                    )
                }
            };
            // Apply to EVERY live session (item 48 semantics): one actor
            // job per session, none blocking another.
            let mut applied: Vec<String> = Vec::new();
            for sid in s.sessions.list() {
                let policy_for_session = policy.clone();
                if let Ok(id) = s
                    .with_sess(Some(&sid), move |sess| {
                        sess.set_normalization_policy(policy_for_session.clone());
                        sess.id.clone()
                    })
                    .await
                {
                    applied.push(id);
                }
            }
            let summary = {
                let mut run = s.run.lock().unwrap();
                run.set_contract(contract.clone(), path.clone());
                json!({
                    "name": contract.schema.name,
                    "version": contract.schema.version,
                    "viewports": contract.viewports.len(),
                    "components": contract.components.len(),
                    "interactions": contract.interactions.len(),
                    "layout_constraints": contract.layout.len(),
                    "oracles": contract.oracles.len(),
                    "volatile_patterns": contract.volatile_patterns.len(),
                    "policy_applied_to_sessions": applied,
                })
            };
            ok(json!({ "loaded": true, "path": path, "contract": summary }))
        }
        // ── validate: document-only, no session needed ──
        CT::Validate => {
            let Some(path) = p.path.clone() else {
                return err(ErrorCategory::InvalidRequest, "validate requires 'path'");
            };
            // Parse WITHOUT the load-time hard stop, so a report of every
            // problem comes back instead of only the first.
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return err(ErrorCategory::InvalidRequest, format!("cannot read: {e}")),
            };
            let parsed: Result<crate::design::ProjectContract, _> = if path.ends_with(".json") {
                serde_json::from_str(&content)
                    .map_err(|e| format!("Failed to parse design contract JSON: {e}"))
            } else {
                serde_yaml::from_str(&content)
                    .map_err(|e| format!("Failed to parse design contract YAML: {e}"))
            };
            match parsed {
                Err(e) => err(ErrorCategory::InvalidRequest, e),
                Ok(contract) => {
                    let results = crate::design::conformance::validate_document(&contract);
                    let verdict = results
                        .iter()
                        .fold(crate::design::Verdict::Pass, |acc, r| acc.merge(r.verdict));
                    ok(json!({
                        "contract": contract.schema.name,
                        "version": contract.schema.version,
                        "verdict": verdict.as_str(),
                        "results": results,
                    }))
                }
            }
        }
        // ── status: full conformance check against the running app ──
        CT::Status => {
            let contract = {
                let run = s.run.lock().unwrap();
                match run.contract() {
                    Some(c) => c.clone(),
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            "no contract loaded; call tui_contract action=load path=... first",
                        )
                    }
                }
            };
            // Item 32: the mode selector is typed — a typo ("strcit")
            // is an invalid_request naming the accepted set, never a
            // silently-ignored override.
            let mode_override = match contract_mode_override(&p.mode) {
                Ok(m) => m,
                Err(e) => return err(ErrorCategory::InvalidRequest, e),
            };
            s.check_contract_against(p.id.as_deref(), contract, mode_override)
                .await
        }
        // ── compare: run conformance now, diff against the baseline ──
        CT::Compare => {
            let contract = {
                let run = s.run.lock().unwrap();
                match run.contract() {
                    Some(c) => c.clone(),
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            "no contract loaded; call tui_contract action=load path=... first",
                        )
                    }
                }
            };
            let mode_override = match contract_mode_override(&p.mode) {
                Ok(m) => m,
                Err(e) => return err(ErrorCategory::InvalidRequest, e),
            };
            let current = match s
                .check_contract_inner(p.id.as_deref(), contract.clone(), mode_override)
                .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return err(ErrorCategory::BackendError, e.to_string()),
                Err(e) => return e,
            };
            let label = p.label.clone().unwrap_or_else(|| "current".into());
            let mut run = s.run.lock().unwrap();
            let baseline_label = p.baseline.clone().unwrap_or_else(|| "baseline".into());
            let baseline = run.contract_baselines().get(&baseline_label).cloned();
            // Record the current result under its label for future compares.
            run.record_contract_baseline(&label, &current);
            match baseline {
                None => ok(json!({
                    "baseline": baseline_label,
                    "baseline_available": false,
                    "note": format!("no baseline '{baseline_label}' recorded yet; this run's result is now stored as '{label}' — fix what you need to fix, then compare again"),
                    "current": current.summary(),
                    "current_results": current.results,
                    "regressions": [],
                    "fixed": [],
                })),
                Some(base) => {
                    let (regressions, fixed) = diff_contract_reports(&base, &current);
                    let verdict = if !regressions.is_empty() {
                        crate::design::Verdict::Fail
                    } else {
                        current.verdict
                    };
                    // Failed comparisons become findings (item 49).
                    if !regressions.is_empty() {
                        let findings: Vec<crate::audit::Finding> = regressions
                            .iter()
                            .map(|(name, before, after)| crate::audit::Finding {
                                id: "CONTRACT-REGRESSION".into(),
                                rule_id: None,
                                severity: "error".into(),
                                category: "contract/compare".into(),
                                summary: format!(
                                    "{name}: was {} ({}), now {} ({})",
                                    before.verdict.as_str(),
                                    before.detail,
                                    after.verdict.as_str(),
                                    after.detail
                                ),
                                evidence: vec![crate::audit::EvidenceRef::point(
                                    crate::audit::EvidenceKind::Other,
                                    "contract_compare",
                                    name.clone(),
                                )
                                .with_detail(json!({
                                    "contract": contract.schema.name,
                                    "baseline": baseline_label,
                                    "check": name,
                                    "before": before.detail,
                                    "after": after.detail,
                                }))],
                                confidence: 1.0,
                                reproduction: None,
                                source_refs: Vec::new(),
                            })
                            .collect();
                        let _ = run.extend_findings(findings);
                    }
                    ok(json!({
                        "baseline": baseline_label,
                        "baseline_available": true,
                        "current": current.summary(),
                        "baseline_summary": base.summary(),
                        "verdict": verdict.as_str(),
                        "regressions": regressions.iter().map(|(n, b, a)| json!({
                            "check": n,
                            "before": b.verdict.as_str(),
                            "before_detail": b.detail,
                            "after": a.verdict.as_str(),
                            "after_detail": a.detail,
                        })).collect::<Vec<_>>(),
                        "fixed": fixed.iter().map(|(n, b, a)| json!({
                            "check": n,
                            "before": b.verdict.as_str(),
                            "after": a.verdict.as_str(),
                            "after_detail": a.detail,
                        })).collect::<Vec<_>>(),
                    }))
                }
            }
        }
        // ── scaffold (Wave 5 item 43): starter contract from the
        // observed UI ──
        CT::Scaffold => {
            let selector = p.id.clone();
            let scaffolded = s
                .with_sess(selector.as_deref(), |sess| {
                    let (screen, sem, _tree, _report) =
                        sess.observe_fused(60).map_err(|e| e.to_string())?;
                    Ok::<_, String>(crate::design::ProjectContract::scaffold_from(&screen, &sem))
                })
                .await;
            let contract = match scaffolded {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return err(ErrorCategory::BackendError, e),
                Err(e) => return e,
            };
            let yaml = match serde_yaml::to_string(&contract) {
                Ok(y) => y,
                Err(e) => return err(ErrorCategory::InternalError, e.to_string()),
            };
            ok(json!({
                "action": "scaffold",
                "inferred": true,
                "contract_name": contract.schema.name,
                "components": contract.components.len(),
                "oracles": contract.oracles.len(),
                "viewports": contract.viewports.iter().map(|v| json!({"cols": v.cols, "rows": v.rows})).collect::<Vec<_>>(),
                "note": "scaffolded from ONE observed frame — everything declared was seen, nothing is yet required. Edit required=true / mode=validation as you fix intent, then tui_contract action=validate.",
                "yaml": yaml,
            }))
        }
        // ── baseline (re-review item 31): run conformance NOW and
        // store the report under an explicit label. The status action
        // no longer silently overwrites "baseline" on every check —
        // baselines are named on purpose, and compare diffs against
        // the label the caller chose.
        CT::Baseline => {
            let contract = {
                let run = s.run.lock().unwrap();
                match run.contract() {
                    Some(c) => c.clone(),
                    None => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            "no contract loaded; call tui_contract action=load path=... first",
                        )
                    }
                }
            };
            let mode_override = match contract_mode_override(&p.mode) {
                Ok(m) => m,
                Err(e) => return err(ErrorCategory::InvalidRequest, e),
            };
            let report = match s
                .check_contract_inner(p.id.as_deref(), contract, mode_override)
                .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return err(ErrorCategory::BackendError, e.to_string()),
                Err(e) => return e,
            };
            let label = p
                .baseline
                .clone()
                .or_else(|| p.label.clone())
                .unwrap_or_else(|| "baseline".into());
            let verdict = report.verdict.as_str().to_string();
            let summary = report.summary();
            {
                let mut run = s.run.lock().unwrap();
                run.record_contract_baseline(&label, &report);
            }
            ok(json!({
                "action": "baseline",
                "label": label,
                "verdict": verdict,
                "summary": summary,
                "note": format!(
                    "stored under '{label}'; tui_contract action=compare baseline={label} diffs against it"
                ),
            }))
        }
    }
}
