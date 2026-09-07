//! tui_contract: project-contract load/validate/compare/scaffold.

pub(crate) mod check;

use crate::audit::{Category, Severity};
use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use check::{contract_mode_override, diff_contract_reports};
use rmcp::serde_json::json;

/// Beta-audit P0.7: ONE mutation-authorization rule for every
/// conformance-running contract action — passive unless the caller
/// explicitly passes allow_mutation=true. `tui_audit profile=contract`
/// derives its policy from the same knob via SafetyPolicy.
fn exec_policy(p: &TuiContractParams) -> crate::design::conformance::ExecPolicy {
    if p.allow_mutation.unwrap_or(false) {
        crate::design::conformance::ExecPolicy::Driving
    } else {
        crate::design::conformance::ExecPolicy::Passive
    }
}

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
            // Beta-audit P1.5: a run represents ONE TUI project. The
            // contract's normalization policy applies ONLY to sessions
            // whose launch cwd resolves to the same project root as the
            // contract-bearing directory — a foreign-root session (a
            // different application in the same run) is never silently
            // normalized by another app's policy. Foreign sessions are
            // listed, not hidden.
            let contract_root = {
                let run = s.run.lock().unwrap();
                let anchor = p
                    .id
                    .as_deref()
                    .and_then(|sid| run.launch_spec(sid))
                    .and_then(|spec| spec.cwd.clone())
                    .or_else(|| run.primary_session_cwd().map(str::to_string));
                anchor.map(|cwd| {
                    crate::session::ProjectLocator::locate(None, &cwd)
                        .root()
                        .to_string()
                })
            };
            let mut applied: Vec<String> = Vec::new();
            let mut skipped_foreign: Vec<String> = Vec::new();
            for sid in s.sessions.list() {
                // Same-project check: this session's launch cwd must
                // resolve to the contract's project root.
                let same_project = {
                    let run = s.run.lock().unwrap();
                    let session_root =
                        run.launch_spec(&sid)
                            .and_then(|spec| spec.cwd.clone())
                            .map(|cwd| {
                                crate::session::ProjectLocator::locate(None, &cwd)
                                    .root()
                                    .to_string()
                            });
                    match (&contract_root, session_root) {
                        // No anchor at all: no scoping info — apply (the
                        // legacy single-project behavior) and say so.
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(a), Some(b)) => a == &b,
                    }
                };
                if !same_project {
                    skipped_foreign.push(sid);
                    continue;
                }
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
                    "policy_skipped_foreign_sessions": skipped_foreign,
                    "project_root_scope": contract_root,
                    "scoping_note": if skipped_foreign.is_empty() {
                        None
                    } else {
                        Some("foreign-root sessions were NOT normalized by this contract's policy — they belong to a different project".to_string())
                    },
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
            // Beta-audit P0.7: passive by default — a status check never
            // drives the app unless the caller explicitly consents.
            let policy = exec_policy(&p);
            check::check_contract_against(s, p.id.as_deref(), contract, mode_override, policy).await
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
            let policy = exec_policy(&p);
            let current = match check::check_contract_inner(
                s,
                p.id.as_deref(),
                contract.clone(),
                mode_override,
                policy,
            )
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
                                severity: Severity::Error,
                                category: Category::Other("contract/compare".into()),
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
                                occurrence_id: None,
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
        // ── scaffold (Wave 5 item 43 + finding 37): starter contract
        // from the observed UI — one frame (mode=current) or a bounded
        // SAFE multi-state pass (mode=explore) ──
        CT::Scaffold => {
            use crate::mcp::params::ScaffoldMode as SM;
            let scaffold_mode = match p.scaffold_mode.as_ref() {
                None => SM::Current,
                Some(crate::mcp::params::Known::Known(m)) => *m,
                Some(crate::mcp::params::Known::Other(o)) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "unknown scaffold_mode '{}' (expected one of: {})",
                            o,
                            <SM as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                        ),
                    );
                }
            };
            let selector = p.id.clone();
            let run = s.run.clone();
            if scaffold_mode == SM::Explore {
                // The multi-state pass DRIVES the app (Tab / resize),
                // so the human control lease gates it exactly like
                // every other driver. (Beta-audit P0.6: Escape is no
                // longer in the pass — it is not state-preserving in
                // general.)
                let scaffolded = s
                    .with_sess(selector.as_deref(), move |sess| {
                        if let Some(refused) = crate::mcp::helpers::lease_refused(sess) {
                            return Err(refused);
                        }
                        let (screen, sem, _tree, _report) = sess
                            .observe_fused(60)
                            .map_err(|e| e.to_string())
                            .map_err(|e| err(ErrorCategory::BackendError, e))?;
                        let (cols, rows) = (sess.cols(), sess.rows());
                        // The pass's session touches, as ONE object: drive
                        // through the ONE pipeline (origin=scaffold) so
                        // evidence + ledger stay uniform with every other
                        // driver; observations go through the fused
                        // authority.
                        struct ScaffoldSession<'a> {
                            sess: &'a mut crate::session::Session,
                            run: &'a std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>,
                            ticket: crate::execution::RunTicket,
                        }
                        impl crate::design::scaffold::ScaffoldIo for ScaffoldSession<'_> {
                            fn observe(
                                &mut self,
                                idle_ms: u64,
                            ) -> anyhow::Result<(
                                crate::screen::ScreenState,
                                crate::semantic::SemanticScreen,
                            )> {
                                let (s, sem, _, _) = self.sess.observe_fused(idle_ms)?;
                                Ok((s, sem))
                            }
                            fn act(
                                &mut self,
                                name: &str,
                                action: crate::execution::CanonicalAction,
                            ) -> anyhow::Result<()> {
                                let spec = crate::execution::CoreDriveSpec {
                                    action: &action,
                                    quiet_ms: 120,
                                    budget_ms: 1500,
                                    no_wait: false,
                                    visibility: crate::execution::InputVisibility::Normal,
                                    completion: crate::capture::CompletionPolicy::StableScreen,
                                    guard: None,
                                    scenario: None,
                                    origin: crate::execution::DriveOrigin::Scaffold,
                                    ticket: self.ticket.clone(),
                                };
                                crate::execution::drive_pipeline(self.sess, self.run, spec)
                                    .map(|_| ())
                                    .map_err(|e| anyhow::anyhow!("{name} failed: {e}"))
                            }
                        }
                        let mut io = ScaffoldSession {
                            sess,
                            run: &run,
                            ticket: crate::execution::RunTicket::capture(&run),
                        };
                        let gathered = crate::design::scaffold::gather_states(
                            (screen, sem),
                            cols,
                            rows,
                            crate::design::scaffold::ScaffoldBudget::default(),
                            &mut io,
                        )
                        .map_err(|e| err(ErrorCategory::BackendError, e.to_string()))?;
                        Ok::<_, rmcp::model::CallToolResult>(
                            crate::design::scaffold::scaffold_multi_state(&gathered, (cols, rows)),
                        )
                    })
                    .await;
                let contract = match scaffolded {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => return e,
                    Err(e) => return e,
                };
                let yaml = match serde_yaml::to_string(&contract) {
                    Ok(y) => y,
                    Err(e) => return err(ErrorCategory::InternalError, e.to_string()),
                };
                // The extension blob names the states, so the response can
                // cite what the pass actually saw.
                let ext = contract
                    .schema
                    .extensions
                    .get("scaffold.inferred")
                    .cloned()
                    .unwrap_or_default();
                let state_count = ext["states"].as_array().map(Vec::len).unwrap_or(0);
                ok(json!({
                    "action": "scaffold",
                    "scaffold_mode": "explore",
                    "inferred": true,
                    "contract_name": contract.schema.name,
                    "states_observed": state_count,
                    "components": contract.components.len(),
                    "interactions": contract.interactions.len(),
                    "oracles": contract.oracles.len(),
                    "viewports": contract.viewports.iter().map(|v| json!({"cols": v.cols, "rows": v.rows})).collect::<Vec<_>>(),
                    "states": ext["states"],
                    "focus_order": ext["states"].as_array().map(|_| ()),
                    "candidate_invariants": ext["candidate_invariants"],
                    "clipping_evidence": ext["clipping_evidence"],
                    "note": "scaffolded from a bounded exploratory pass (initial screen, Tab focus walk, viewport probes; NO Escape — it is not state-preserving in general). Everything declared was SEEN, nothing is yet required, and the focus invariants are UNVERIFIED candidates — see candidate_invariants. Promote deliberately (required=true / mode=validation), then tui_contract action=check to prove the candidates.",
                    "yaml": yaml,
                }))
            } else {
                let scaffolded = s
                    .with_sess(selector.as_deref(), |sess| {
                        let (screen, sem, _tree, _report) =
                            sess.observe_fused(60).map_err(|e| e.to_string())?;
                        Ok::<_, String>(crate::design::ProjectContract::scaffold_from(
                            &screen, &sem,
                        ))
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
                    "scaffold_mode": "current",
                    "inferred": true,
                    "contract_name": contract.schema.name,
                    "components": contract.components.len(),
                    "oracles": contract.oracles.len(),
                    "viewports": contract.viewports.iter().map(|v| json!({"cols": v.cols, "rows": v.rows})).collect::<Vec<_>>(),
                    "note": "scaffolded from ONE observed frame — everything declared was seen, nothing is yet required. Edit required=true / mode=validation as you fix intent, then tui_contract action=validate. mode=explore gathers a multi-state pass.",
                    "yaml": yaml,
                }))
            }
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
            let policy = exec_policy(&p);
            let report = match check::check_contract_inner(
                s,
                p.id.as_deref(),
                contract,
                mode_override,
                policy,
            )
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
