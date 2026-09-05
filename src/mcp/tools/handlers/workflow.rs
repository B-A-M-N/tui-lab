//! tui_workflow (finding 38): the construction-oriented workflow object.
//!
//! The composition the finding asks for — Finding → component identity →
//! source ref → framework context → contract expectation → minimal
//! reproduction → targeted validation — is a JOIN over primitives that
//! already exist, served as ONE object per finding so an agent stops
//! chaining five tools to assemble it. Nothing here is autonomous: every
//! field cites the run's own evidence, `inspect`/`diagnose` never drive,
//! and `verify` runs exactly the plan the finding's own evidence
//! declares (replay + targeted re-checks), under the human lease.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_workflow`: the #[tool] method in `super` decodes params
/// and delegates here.
pub(crate) async fn tui_workflow(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiWorkflowParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::WorkflowAction as WA;
    let Some(action) = (match &p.action {
        crate::mcp::params::Known::Known(a) => Some(*a),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown workflow action '{}' (expected one of: {})",
                    o,
                    <WA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    match action {
        WA::Inspect => inspect_finding(s, &p).await,
        WA::Verify => verify_finding(s, &p).await,
        WA::Diagnose => diagnose_all(s, &p).await,
    }
}

/// The framework context slice: what the project actually is, at the
/// resolution the finding investigation cares about (primary = the app
/// framework; terminal_io/styling are architecture context, not
/// framework claims).
fn framework_context(cwd: Option<&str>) -> serde_json::Value {
    let cwd = cwd.unwrap_or(".");
    let project = crate::session::ProjectLocator::locate(None, cwd);
    let det = crate::framework::detect::detect(project.root());
    let primary = det.primary.as_ref().map(|c| {
        json!({
            "name": c.name,
            "class": c.class,
            "confidence": c.confidence,
            "evidence": c.evidence,
        })
    });
    json!({
        "project_root": project.root(),
        "primary": primary,
        "terminal_io": det.terminal_io,
        "styling": det.styling,
        "native_adapter_declared": det.native_adapter,
        "note": "detection scans manifests; a framework named here is build evidence, not a runtime attestation (tui_framework action=capabilities for the live channel)",
    })
}

/// The contract expectation slice: what the loaded contract declares
/// about this finding's targets (component roles that match, oracle
/// expressions citing them).
fn contract_expectation(
    contract: Option<&crate::design::ProjectContract>,
    targets: &[String],
) -> serde_json::Value {
    let Some(contract) = contract else {
        return json!({
            "loaded": false,
            "expectation": serde_json::Value::Null,
        });
    };
    // Components whose role or name matches any evidence target, and
    // oracle expressions whose text cites one.
    let mut components = Vec::new();
    for comp in &contract.components {
        let hit = targets.iter().any(|t| {
            let tl = t.to_lowercase();
            comp.role.to_lowercase().contains(&tl)
                || comp.name.to_lowercase().contains(&tl)
                || t.to_lowercase().contains(&comp.name.to_lowercase())
        });
        if hit {
            components.push(json!({
                "name": comp.name,
                "role": comp.role,
                "required": comp.required,
            }));
        }
    }
    let mut oracles = Vec::new();
    for o in &contract.oracles {
        if targets.iter().any(|t| o.expr.contains(t.as_str())) {
            oracles.push(json!({ "id": o.id, "expr": o.expr }));
        }
    }
    json!({
        "loaded": true,
        "name": contract.schema.name,
        "version": contract.schema.version,
        "mode": contract.schema.mode.as_str(),
        "components": components,
        "oracles": oracles,
        "expectation": if components.is_empty() && oracles.is_empty() {
            serde_json::Value::Null
        } else {
            json!("the contract names material this finding touches — a fix that changes these components/oracles should update the contract with them")
        },
    })
}

/// Component identity for the finding's evidence targets: semantic ids
/// as-is, plus the native id + source loci the run can attest.
fn component_identity(finding: &crate::audit::Finding) -> serde_json::Value {
    let targets: Vec<String> = finding
        .evidence
        .iter()
        .filter_map(|e| e.target.clone())
        .collect();
    let mut identities = Vec::new();
    for t in &targets {
        // The finding's own loci for this target (already joined with the
        // coverage ledger's app-attested chain by the caller's
        // `join_source_refs_if_known`), tiered by the actionable fence.
        let loci: Vec<_> = finding
            .source_refs
            .iter()
            .filter(|sr| {
                sr.location().contains(t.as_str())
                    || sr
                        .framework_id
                        .as_deref()
                        .map(|f| t.contains(f))
                        .unwrap_or(false)
                    || t.contains(&sr.location())
            })
            .cloned()
            .collect();
        identities.push(json!({
            "semantic_target": t,
            "source_refs": loci.iter().map(|sr| json!({
                "location": sr.location(),
                "symbol": sr.symbol,
                "framework_id": sr.framework_id,
                "confidence": sr.confidence,
                "source": sr.source,
                "provenance": sr.provenance.name(),
                "actionable": sr.is_actionable(),
            })).collect::<Vec<_>>(),
            "loci_known": !loci.is_empty(),
        }));
    }
    json!({ "identities": identities })
}

/// One finding's full chain (finding 38's diagram, realized).
fn workflow_chain(
    run: &crate::run::RunContext,
    finding: &crate::audit::Finding,
    framework: &serde_json::Value,
) -> serde_json::Value {
    let targets: Vec<String> = finding
        .evidence
        .iter()
        .filter_map(|e| e.target.clone())
        .collect();
    // Component identity (loci joined late, like tui_explain does).
    let joined = run.join_source_refs_if_known(finding);
    let identity = component_identity(&joined);
    // Contract expectation.
    let expectation = contract_expectation(run.contract(), &targets);
    // Minimal reproduction (as recorded; verify replays it).
    let reproduction = match &finding.reproduction {
        Some(scen_id) => match run.load_scenario(scen_id) {
            Ok(sc) => json!({
                "scenario_id": sc.id,
                "scenario_name": sc.name,
                "steps": sc.step_count(),
                "note": "replay with tui_workflow action=verify",
            }),
            Err(_) => json!({
                "scenario_id": scen_id,
                "missing": true,
                "note": "the finding cites a reproduction this run cannot load (scenario evicted or from a prior run)",
            }),
        },
        None => json!(null),
    };
    // Targeted validation: the diagnostic context's verification plan.
    let sessions: Vec<String> = run.launch_specs().into_iter().map(|(sid, _)| sid).collect();
    let (contexts, _) = run.diagnostic_contexts();
    let verification = contexts
        .into_iter()
        .find(|c| c.finding.id == finding.id)
        .map(|c| {
            json!({
                "summary": c.verification.summary,
                "targeted_checks": c.verification.targeted_checks,
                "replay": c.verification.replay,
                "next_observations": c.suggested_next_observations,
            })
        })
        .unwrap_or(serde_json::Value::Null);

    json!({
        "workflow": "construction",
        "finding": finding,
        "explanation": crate::terminal::explain::explain_finding(finding, None, None),
        "component_identity": identity,
        "framework": framework,
        "contract": expectation,
        "reproduction": reproduction,
        "verification": verification,
        "run_id": run.id,
        "sessions": sessions,
    })
}

/// inspect: one finding's chain (no driving).
async fn inspect_finding(
    s: &crate::mcp::tools::TuiLabServer,
    p: &TuiWorkflowParams,
) -> rmcp::model::CallToolResult {
    let Some(finding_id) = p.finding_id.clone() else {
        return err(
            ErrorCategory::InvalidRequest,
            "inspect requires 'finding_id' (from tui_audit or tui://findings)",
        );
    };
    let framework = framework_context(p.cwd.as_deref());
    let chain = {
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
        let joined = run.join_source_refs_if_known(finding);
        workflow_chain(&run, &joined, &framework)
    };
    ok(chain)
}

/// diagnose: every finding's chain (list-level; no driving).
async fn diagnose_all(
    s: &crate::mcp::tools::TuiLabServer,
    p: &TuiWorkflowParams,
) -> rmcp::model::CallToolResult {
    let framework = framework_context(p.cwd.as_deref());
    let (chains, skipped) = {
        let run = s.run.lock().unwrap();
        let joined: Vec<_> = run
            .findings()
            .iter()
            .map(|f| run.join_source_refs_if_known(f))
            .collect();
        let chains: Vec<serde_json::Value> = joined
            .iter()
            .map(|f| workflow_chain(&run, f, &framework))
            .collect();
        let skipped = run.findings().len() - joined.len();
        (chains, skipped)
    };
    if chains.is_empty() {
        return ok(json!({
            "workflow": "construction",
            "findings": [],
            "skipped": 0,
            "note": "no findings recorded in this run — run an audit first (tui_audit action=run)",
        }));
    }
    ok(json!({
        "workflow": "construction",
        "findings": chains,
        "skipped": skipped,
    }))
}

/// verify: run the finding's verification plan LIVE — replay the
/// reproduction scenario when present, then the targeted re-checks
/// (re-run the cheapest audit surface over the finding's target) — and
/// report whether the finding still reproduces. Lease-gated.
async fn verify_finding(
    s: &crate::mcp::tools::TuiLabServer,
    p: &TuiWorkflowParams,
) -> rmcp::model::CallToolResult {
    let Some(finding_id) = p.finding_id.clone() else {
        return err(
            ErrorCategory::InvalidRequest,
            "verify requires 'finding_id'",
        );
    };
    // Resolve the finding snapshot, plan, and the re-check surface BEFORE
    // driving (pure reads). The surface is the audit profile named by the
    // finding's own category slug when one exists, else the `full`
    // composite (which under safe-only runs its observational members and
    // names the withheld ones).
    let (finding_snapshot, scenario_id, plan, recheck_profile) = {
        let run = s.run.lock().unwrap();
        let Some(finding) = run.findings().iter().find(|f| f.id == finding_id) else {
            return err(
                ErrorCategory::InvalidRequest,
                format!("unknown finding id '{finding_id}' in run '{}'", run.id),
            );
        };
        let (contexts, _) = run.diagnostic_contexts();
        let plan = contexts
            .into_iter()
            .find(|c| c.finding.id == finding_id)
            .map(|c| c.verification);
        let category_slug = finding.category.as_str().to_string();
        let profile = crate::audit::orchestrator::AuditProfile::parse(&category_slug)
            .map(|p| p.name().to_string())
            .unwrap_or_else(|_| "full".to_string());
        (finding.clone(), finding.reproduction.clone(), plan, profile)
    };
    if plan.is_none() && scenario_id.is_none() {
        return err(
            ErrorCategory::InvalidRequest,
            format!(
                "finding '{finding_id}' has no verification plan (no evidence targets) and no recorded reproduction — nothing to verify against"
            ),
        );
    };

    // The replay leg (when the finding has a reproduction): lease-gated,
    // through the standard scenario runner inside the run context.
    let replay_result: Option<serde_json::Value> = match &scenario_id {
        None => None,
        Some(scen_id) => {
            let scenario = {
                let run = s.run.lock().unwrap();
                match run.load_scenario(scen_id) {
                    Ok(sc) => Some(sc),
                    Err(e) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!("reproduction scenario '{scen_id}' cannot load: {e}"),
                        )
                    }
                }
            };
            let scenario = scenario.expect("checked above");
            let run = s.run.clone();
            let selector = p.id.clone();
            let scen = scenario.clone();
            let result = s
                .with_sess(selector.as_deref(), move |sess| {
                    if let Some(refused) = lease_refused(sess) {
                        return Err(refused);
                    }
                    let report = crate::scenario::runner::ScenarioRunner::run_in_run_with_policy(
                        &scen,
                        sess,
                        &[],
                        Some(&run),
                        None,
                    );
                    Ok(json!({
                        "status": report.status,
                        "steps_total": report.steps_total,
                        "steps_passed": report.steps_passed,
                        "steps_failed": report.steps_failed,
                        "steps_skipped": report.steps_skipped,
                    }))
                })
                .await
                .and_then(|inner| inner);
            match result {
                Ok(v) => Some(v),
                Err(e) => return e,
            }
        }
    };

    // The targeted leg: re-run the finding's audit surface LIVE (one
    // observation pass — the same engine entry point the original audit
    // used) and match its fresh findings against this finding by rule
    // identity. Lease-gated: a driving profile touches the app; under the
    // safe-only default an invasive surface is WITHHELD by the engine (an
    // ORCH-GATED row), which the match reports as `not_rechecked` — never
    // as a pass.
    let recheck: Option<serde_json::Value> = match &plan {
        Some(_) => {
            let run = s.run.clone();
            let profile = recheck_profile.clone();
            let rule_key = finding_snapshot
                .rule_id
                .clone()
                .unwrap_or_else(|| finding_snapshot.id.clone());
            let result = s
                .with_sess(p.id.as_deref(), move |sess| {
                    if let Some(refused) = lease_refused(sess) {
                        return Err(refused);
                    }
                    let contract = run.lock().unwrap().contract().cloned();
                    match crate::audit::orchestrator::run_profile_checked(
                        sess,
                        &profile,
                        contract.as_ref(),
                        crate::audit::orchestrator::SafetyPolicy::SafeOnly,
                    ) {
                        Ok(report) => {
                            let gated = report
                                .findings
                                .iter()
                                .any(|f| f.id == "ORCH-GATED" && f.summary.contains("was not run"));
                            let refired = report.findings.iter().any(|f| {
                                f.rule_id.as_deref() == Some(rule_key.as_str())
                                    || f.occurrence_id == finding_snapshot.occurrence_id
                            });
                            Ok(json!({
                                "profile": report.profile.name(),
                                "mode": report.mode,
                                "engine_gated": gated,
                                "rule_refired": if gated { serde_json::Value::Null } else { json!(refired) },
                                "fresh_finding_count": report.findings.len(),
                            }))
                        }
                        Err(msg) => Err(err(ErrorCategory::InvalidRequest, msg)),
                    }
                })
                .await
                .and_then(|inner| inner);
            match result {
                Ok(v) => Some(v),
                Err(e) => return e,
            }
        }
        None => None,
    };

    // The still-reproduces verdict. Two evidence legs, honestly weighted:
    //   replay    — did the reproduction scenario fail again?
    //   recheck   — did the finding's rule fire again on a fresh audit pass?
    // Each leg answers only when it RAN; a gated/withheld recheck is
    // `null` with the reason, never a pass.
    let replay_verdict = match &replay_result {
        None => serde_json::Value::Null,
        Some(replay) => {
            let failed = replay["steps_failed"].as_u64().unwrap_or(0);
            let skipped = replay["steps_skipped"].as_u64().unwrap_or(0);
            if failed > 0 || skipped > 0 {
                json!({
                    "still_reproduces": null,
                    "reason": "the replay itself did not complete cleanly (failed/skipped steps) — it proves nothing either way; fix the replay path first",
                })
            } else {
                json!({
                    "still_reproduces": null,
                    "reason": "the replay completed cleanly but step-level success does not re-evaluate the finding's rule — the targeted recheck is the rule-level leg",
                })
            }
        }
    };
    let recheck_verdict = match &recheck {
        None => json!({
            "still_reproduces": null,
            "reason": "no targeted recheck ran (the finding has no evidence targets) — only the replay leg ran",
        }),
        Some(r) if r["engine_gated"].as_bool() == Some(true) => json!({
            "still_reproduces": null,
            "reason": format!(
                "profile '{}' was withheld by the safe-only gate — pass allow_mutation=true via tui_audit to recheck an invasive surface",
                recheck_profile
            ),
        }),
        Some(r) => json!({
            "still_reproduces": r["rule_refired"].clone(),
            "reason": if r["rule_refired"].as_bool() == Some(true) {
                "the finding's rule fired again on a fresh pass of its audit surface".to_string()
            } else {
                format!("a fresh pass of profile '{}' produced no instance of this finding's rule", recheck_profile)
            },
        }),
    };

    ok(json!({
        "workflow": "verify",
        "finding_id": finding_id,
        "recheck_profile": recheck_profile,
        "replay": replay_result,
        "recheck": recheck,
        "verdict": {
            "replay": replay_verdict,
            "recheck": recheck_verdict,
        },
        "plan_summary": plan.map(|pl| json!({"summary": pl.summary, "replay": pl.replay})),
    }))
}
