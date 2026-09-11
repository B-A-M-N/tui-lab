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
        WA::Construct => construct_view(s, &p).await,
        WA::Inspect => inspect_finding(s, &p).await,
        WA::Verify => verify_finding(s, &p).await,
        WA::Diagnose => diagnose_all(s, &p).await,
    }
}

/// The framework context slice: what the project actually is, at the
/// resolution the finding investigation cares about (primary = the app
/// framework; terminal_io/styling are architecture context, not
/// framework claims).
///
/// Audit P1 (finding 14): the root is derived from the RUN's recorded
/// `LaunchSpec.cwd` — where the audited app actually lives — never from
/// this server process's cwd. An explicit `cwd` parameter is a conscious
/// override and is reported as such.
///
/// Beta-audit P1.3: with NO explicit cwd and NO recorded launch cwd,
/// the answer is `unknown` with `requires: "cwd"` — never detection run
/// over the server's own directory. Evidence from an unrelated tree (the
/// server's own repository) joined into a target TUI investigation is
/// worse than no evidence: an agent may act on it. There is no fallback.
fn framework_context(recorded_cwd: Option<&str>, explicit_cwd: Option<&str>) -> serde_json::Value {
    let (root, provenance) = match explicit_cwd {
        Some(c) => (c.to_string(), "explicit_override".to_string()),
        // The primary session's recorded launch cwd is the app's home.
        None => match recorded_cwd {
            Some(c) => (c.to_string(), "recorded_launch_cwd".to_string()),
            None => {
                return json!({
                    "primary": serde_json::Value::Null,
                    "root_provenance": "unknown",
                    "requires": "cwd",
                    "note": "no launch cwd recorded for this run and no explicit cwd given — framework detection refuses to guess (it would otherwise scan the server's own directory, an unrelated project). Pass cwd= to inspect a specific tree.",
                });
            }
        },
    };
    let project = crate::session::ProjectLocator::locate(None, &root);
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
        "root_provenance": provenance,
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

/// Component identity for the finding's evidence targets (beta-audit
/// P1.4): a STRUCTURED join, not string containment. Semantic control
/// ids (`button/save`), native framework ids (`#save`), and source
/// locations (`src/ui.rs:184`) are different namespaces — one string
/// happening to contain another proves nothing. The join key is the one
/// the run actually established: the coverage ledger's
/// control↔coverage-target match (the same matcher the provenance join
/// used to attach the loci), so each identity block names the semantic
/// id, the native id, and the source loci THAT identity is known to
/// have — and loci that cannot be pinned to one target ride at
/// `provenance_scope: "finding"` instead of being dropped or guessed.
fn component_identity(
    run: &crate::run::RunContext,
    finding: &crate::audit::Finding,
) -> serde_json::Value {
    let targets: Vec<String> = finding
        .evidence
        .iter()
        .filter_map(|e| e.target.clone())
        .collect();
    let sr_json = |sr: &crate::semantic::source_ref::SourceRef| {
        json!({
            "location": sr.location(),
            "symbol": sr.symbol,
            "framework_id": sr.framework_id,
            "confidence": sr.confidence,
            "source": sr.source,
            "provenance": sr.provenance.name(),
            "actionable": sr.is_actionable(),
        })
    };
    let mut identities = Vec::new();
    let mut joined_targets: std::collections::BTreeSet<String> = Default::default();
    for t in &targets {
        // The identity's source loci: only what the run's own provenance
        // machinery attached for THIS control — i.e. loci already on the
        // finding (attested for this finding) whose native id or coverage
        // target matches this control through the ledger's identity
        // match, never a free-text containment between namespaces.
        let native_id = run
            .coverage_ledger()
            .iter()
            .find_map(|(cov_target, entry)| {
                crate::run::coverage_target_matches_control(cov_target, t).then(|| {
                    entry
                        .source_refs
                        .first()
                        .and_then(|sr| sr.framework_id.clone())
                })
            })
            .flatten();
        // P1-46: classify the join method. Exact native/framework ID is
        // the only attested route; the fallback coverage-target slug match
        // is heuristic and stays non-actionable.
        let join_method = native_id
            .as_ref()
            .map(|_| "exact_native_id")
            .unwrap_or("coverage_target_heuristic");
        let confidence = match join_method {
            "exact_native_id" => 1.0,
            _ => 0.7,
        };
        let loci: Vec<_> = finding
            .source_refs
            .iter()
            .filter(|sr| match &sr.framework_id {
                // A locus joins this target when its native framework id
                // is the one the ledger bound to this control.
                Some(fid) => native_id.as_deref() == Some(fid.as_str()),
                // No native id on the locus: it can still join when the
                // run attested the locus FOR a coverage target that IS
                // this control (the attested chain carries no separate
                // native id — the locus came from the same ledger entry).
                None => run.coverage_ledger().iter().any(|(cov_target, entry)| {
                    !entry.source_refs.is_empty()
                        && entry
                            .source_refs
                            .iter()
                            .any(|e_sr| e_sr.location() == sr.location())
                        && crate::run::coverage_target_matches_control(cov_target, t)
                }),
            })
            .map(sr_json)
            .collect();
        if !loci.is_empty() {
            joined_targets.insert(t.clone());
        }
        identities.push(json!({
            "semantic_target": t,
            "native_id": native_id,
            "join_method": join_method,
            "join_confidence": confidence,
            "source_refs": loci,
            "loci_known": !loci.is_empty(),
            "provenance_scope": "target",
        }));
    }
    // Finding-scoped provenance: loci the finding carries that could NOT
    // be pinned to one evidence target. They are real provenance (kept),
    // labeled honestly as finding-level rather than silently dropped.
    let finding_scoped: Vec<_> = finding
        .source_refs
        .iter()
        .filter(|sr| {
            // Not already surfaced under some target above.
            !identities.iter().any(|id| {
                id["source_refs"]
                    .as_array()
                    .map(|arr| arr.iter().any(|j| j["location"] == json!(sr.location())))
                    .unwrap_or(false)
            })
        })
        .map(|sr| {
            let mut j = sr_json(sr);
            j["provenance_scope"] = json!("finding");
            j
        })
        .collect();
    json!({
        "identities": identities,
        "finding_scoped_source_refs": finding_scoped,
        "join": if finding_scoped.is_empty() {
            json!("all loci pinned to a semantic target")
        } else {
            json!("some loci are finding-scoped only — no evidence target pins them")
        },
    })
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
    let identity = component_identity(run, &joined);
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
        "run_id": run.id(),
        "sessions": sessions,
    })
}

/// construct (beta-audit P1.1): the greenfield/understanding entry
/// point. One SAFE call that joins what the agent would otherwise
/// chain six tools for — live session capabilities, the observation
/// inspection (the SAME view `tui_observe mode=inspect` serves),
/// project/framework identity, native adapter status, the loaded
/// contract, observation-derived contract candidates (status
/// `unverified` — never promoted to declared facts), and the exact
/// next invocations. It never drives the app and never edits source.
async fn construct_view(
    s: &crate::mcp::tools::TuiLabServer,
    p: &TuiWorkflowParams,
) -> rmcp::model::CallToolResult {
    // Framework identity first (pure run read; no session).
    let framework = {
        let run = s.run.lock().unwrap();
        framework_context(run.primary_session_cwd(), p.cwd.as_deref())
    };
    // The session live view: capabilities + the shared inspection view.
    // When no session is selected AND the run has none, construct still
    // answers — the run-level joins stand alone, with the session leg
    // reported honestly absent (a bare run is a valid construct call).
    let has_target = p.id.is_some() || s.run.lock().unwrap().primary_launch_spec().is_some();
    let session_view = if !has_target {
        serde_json::Value::Null
    } else {
        let selector = p.id.clone();
        let run_handle = s.run.clone();
        let session_live = s
            .with_sess(selector.as_deref(), move |sess| {
                let caps = sess.capabilities();
                let adapter = sess.adapter_status();
                // The same inspection `tui_observe mode=inspect` serves — one
                // settle cycle, then the extracted view body.
                let sink = crate::execution::RunEvidenceSink::capture(&run_handle);
                let screen = match super::observe::sweep(sess, &sink, 80) {
                    Ok(sc) => sc,
                    Err((c, m)) => {
                        return crate::mcp::helpers::err(c, m);
                    }
                };
                let observe_params = TuiObserveParams {
                    mode: Some(crate::mcp::params::Known::Known(
                        crate::mcp::params::ObserveMode::Inspect,
                    )),
                    idle_ms: None,
                    id: None,
                    consumer: None,
                    query: None,
                    text: None,
                    since_seq: None,
                    until_seq: None,
                    limit: None,
                    event_types: None,
                    target: None,
                };
                let inspect =
                    super::observe_modes::inspect_view(sess, &observe_params, screen, &run_handle);
                let inspect_envelope: serde_json::Value = serde_json::from_str(
                    &inspect
                        .content
                        .first()
                        .map(|c| match c {
                            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default(),
                )
                .unwrap_or(serde_json::Value::Null);
                let inspect_json = inspect_envelope
                    .get("data")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let session_json = json!({
                    "session": sess.id,
                    "generation": sess.generation,
                    "backend": sess.backend_kind.display(),
                    "capabilities": {
                        "mouse": caps.mouse,
                        "kitty_keyboard": caps.kitty_keyboard,
                        "colors": caps.colors,
                        "cell_attributes": caps.cell_attributes,
                        "title": caps.title,
                        "scrollback": caps.scrollback,
                        "bracketed_paste": caps.bracketed_paste,
                        "signals": caps.signals,
                    },
                    "native_adapter": {
                        "adapter_available": adapter.adapter_available,
                        "channel_active": adapter.native_channel_active,
                        "frames_accepted": adapter.frames_received,
                        "frames_invalid": adapter.frames_invalid,
                    },
                    "inspection": inspect_json,
                });
                crate::mcp::helpers::ok(session_json)
            })
            .await;
        match session_live {
            Ok(v) => {
                let text = v
                    .content
                    .first()
                    .map(|c| match c {
                        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                let v: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                // Unwrap the inner tool envelope: construct's packet is ONE
                // object, not an envelope inside an envelope.
                v.get("data").cloned().unwrap_or(serde_json::Value::Null)
            }
            Err(e) => return e,
        }
    };
    // Run-held material: loaded contract, candidate invariants from what
    // the observation shows, untested design properties, next calls.
    let (contract_block, candidates, next_invocations) = {
        let run = s.run.lock().unwrap();
        let contract = run.contract();
        let contract_block = match contract {
            Some(c) => json!({
                "loaded": true,
                "name": c.schema.name,
                "version": c.schema.version,
                "mode": c.schema.mode.as_str(),
                "components": c.components.len(),
                "oracles": c.oracles.len(),
            }),
            None => json!({
                "loaded": false,
                "starter": "tui_contract action=scaffold generates an observation-derived starter contract from the inspection above",
            }),
        };
        // Candidates from what the observation-derived scaffold pass
        // would propose — all `unverified` until a conformance run
        // proves them (P0.5's rule: unverified is never promoted).
        let candidates = json!([
            crate::design::ProjectContract::scaffold_candidate(
                "reverse_tab_required",
                "unverified",
                "drive Shift+Tab after a Tab walk and compare focus order against the reverse of the walk (tui_audit profile=contract allow_mutation=true)",
            ),
            crate::design::ProjectContract::scaffold_candidate(
                "escape_closes_modal",
                "unverified",
                "open a modal, press Escape, observe whether the dialog region closes (tui_intent or tui_audit profile=contract allow_mutation=true)",
            ),
            crate::design::ProjectContract::scaffold_candidate(
                "destructive_require_confirmation",
                "unverified",
                "trigger a destructive action and observe whether a confirmation dialog appears (requires explicit authorization)",
            ),
        ]);
        let next = json!([
            { "call": "tui_observe", "args": { "mode": "inspect" }, "why": "re-inspect after any change" },
            { "call": "tui_contract", "args": { "action": "scaffold" }, "why": "produce the observation-derived starter contract" },
            { "call": "tui_audit", "args": { "profile": "discoverability" }, "why": "passive first audit (no driving)" },
            { "call": "tui_workflow", "args": { "action": "diagnose" }, "why": "once findings exist, per-finding construction chains" },
        ]);
        (contract_block, candidates, next)
    };
    ok(json!({
        "workflow": "construct",
        "note": "one safe join: nothing here drove the app; candidates are UNVERIFIED until a conformance run proves them",
        "session": session_view,
        "framework": framework,
        "contract": contract_block,
        "candidate_invariants": candidates,
        "next_invocations": next_invocations,
    }))
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
    let framework = {
        let run = s.run.lock().unwrap();
        framework_context(run.primary_session_cwd(), p.cwd.as_deref())
    };
    let chain = {
        let run = s.run.lock().unwrap();
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
    let framework = {
        let run = s.run.lock().unwrap();
        framework_context(run.primary_session_cwd(), p.cwd.as_deref())
    };
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
            "note": "no findings recorded in this run — run an audit first (tui_audit profile=...)",
        }));
    }
    ok(json!({
        "workflow": "construction",
        "findings": chains,
        "skipped": skipped,
    }))
}

/// verify: run the finding's verification strategy LIVE and RECORD it.
///
/// Beta-audit P1.2 rewrite. The old version had four stacked defects:
/// a category→profile fallback that degraded unknown categories to an
/// unrelated `full` audit; no mutation authorization (SafeOnly always,
/// so invasive surfaces were permanently gated with the only remedy
/// being "leave the workflow"); rule matching by raw string (fresh
/// findings are not normalized the same way as stored ones); and replay
/// leg wording that implied replay itself verified the finding.
///
/// Now:
/// - strategy: `VerificationStrategy::for_finding` — rule-prefix table,
///   category only when it names an actual profile, never a broad
///   fallback; no engine surface ⇒ honest `no_surface` (replay +
///   targeted observation only).
/// - authorization: `allow_mutation=true` mirrors tui_audit through the
///   SAME centralized policy (`run_profile_checked` +
///   `SafetyPolicy::AllowMutation`); default false reports `gated`.
/// - matching: canonical `compare::fingerprint` (occurrence identity)
///   both ways — the fresh pass is normalized before the match.
/// - replay: reported as `reproduction_setup` — setup for the verdict,
///   never proof.
/// - persistence: a `VerificationRecord` lands in the run, citable
///   later.
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
    let allow_mutation = p.allow_mutation.unwrap_or(false);

    // Resolve the finding snapshot and its strategy BEFORE driving
    // (pure reads).
    let (finding_snapshot, scenario_id, strategy) = {
        let run = s.run.lock().unwrap();
        let Some(finding) = run.findings().iter().find(|f| f.id == finding_id) else {
            return err(
                ErrorCategory::InvalidRequest,
                format!("unknown finding id '{finding_id}' in run '{}'", run.id()),
            );
        };
        (
            finding.clone(),
            finding.reproduction.clone(),
            crate::audit::verification::VerificationStrategy::for_finding(finding),
        )
    };
    let fingerprint = crate::audit::compare::fingerprint(&finding_snapshot);
    let has_replay_leg = scenario_id.is_some() && strategy.needs_reproduction;
    let recheck_profile = strategy
        .audit_profile
        .as_ref()
        .map(|ap| ap.name().to_string());
    if !has_replay_leg && recheck_profile.is_none() {
        return err(
            ErrorCategory::InvalidRequest,
            format!(
                "finding '{finding_id}' has no verification surface: its rule has no re-check profile and it records no reproduction scenario. Verify it by direct observation of its evidence targets instead."
            ),
        );
    }

    // ── Leg 1: reproduction replay (SETUP, not proof) ────────────────
    let replay_result: Option<serde_json::Value> = if has_replay_leg {
        let scen_id = scenario_id.clone().expect("checked above");
        let scenario = {
            let run = s.run.lock().unwrap();
            match run.load_scenario(&scen_id) {
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
                    "scenario_id": scen.id,
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
    } else {
        None
    };

    // ── Leg 2: the strategy's re-check surface ───────────────────────
    // `executed` is Some only when the surface actually ran; `gated`
    // when the caller withheld authorization. Under the human lease the
    // same audit-descriptor rule applies as on tui_audit: observational
    // surfaces stay available, exclusive-control ones refuse.
    let recheck: Option<serde_json::Value> = match &recheck_profile {
        None => None,
        Some(profile_name) => {
            let run = s.run.clone();
            let profile_name = profile_name.clone();
            let rule_key = finding_snapshot
                .rule_id
                .clone()
                .unwrap_or_else(|| finding_snapshot.id.clone());
            let expect_occurrence = finding_snapshot.occurrence_id.clone();
            let fingerprint = fingerprint.clone();
            let exclusive = crate::audit::orchestrator::AuditProfile::parse(&profile_name)
                .map(|ap| ap.requires_exclusive_control())
                .unwrap_or(true);
            let policy = if allow_mutation {
                crate::audit::orchestrator::SafetyPolicy::AllowMutation
            } else {
                crate::audit::orchestrator::SafetyPolicy::SafeOnly
            };
            let result = s
                .with_sess_authorized(p.id.as_deref(), move |sess, _ticket| {
                    if exclusive {
                        if let Some(refused) = lease_refused(sess) {
                            return Err(refused);
                        }
                    }
                    let contract = run.lock().unwrap().contract().cloned();
                    match crate::audit::orchestrator::run_profile_checked(
                        sess,
                        &profile_name,
                        contract.as_ref(),
                        policy,
                    ) {
                        Ok(report) => {
                            // A withheld surface is named by the engine
                            // as ORCH-GATED; that is `gated`, never a
                            // pass and never a refutation.
                            let gated = report
                                .findings
                                .iter()
                                .any(|f| f.id == "ORCH-GATED" && f.summary.contains("was not run"));
                            // Canonical identity match: normalize the
                            // fresh findings to fingerprints and compare
                            // against THIS finding's fingerprint — and
                            // the rule-level fallback (a rule refiring
                            // on a DIFFERENT occurrence is still this
                            // defect's rule firing).
                            let fresh: Vec<String> = report
                                .findings
                                .iter()
                                .map(crate::audit::compare::fingerprint)
                                .collect();
                            let rule_refired = !gated
                                && (fresh.iter().any(|fp| fp == &fingerprint)
                                    || report.findings.iter().any(|f| {
                                        f.rule_id.as_deref() == Some(rule_key.as_str())
                                            || f.id == rule_key
                                            || f.occurrence_id == expect_occurrence
                                    }));
                            Ok(json!({
                                "profile": report.profile.name(),
                                "risk": report.profile.risk().name(),
                                "executed": !gated,
                                "gated": gated,
                                "rule_refired": if gated { serde_json::Value::Null } else { json!(rule_refired) },
                                "fresh_finding_count": report.findings.len(),
                                "generation": sess.generation,
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
    };

    // ── The verdict, honestly weighted ───────────────────────────────
    let replay_verdict = match &replay_result {
        None => json!({
            "leg": "reproduction_setup",
            "ran": false,
            "note": "no reproduction scenario recorded (or the strategy does not need it)",
        }),
        Some(replay) => {
            let failed = replay["steps_failed"].as_u64().unwrap_or(0);
            let skipped = replay["steps_skipped"].as_u64().unwrap_or(0);
            if failed > 0 || skipped > 0 {
                json!({
                    "leg": "reproduction_setup",
                    "ran": true,
                    "clean": false,
                    "note": "the replay did not complete cleanly (failed/skipped steps) — it proves nothing either way; fix the replay path first",
                })
            } else {
                json!({
                    "leg": "reproduction_setup",
                    "ran": true,
                    "clean": true,
                    "note": "replay completed cleanly — reproduction SETUP for the re-check, not proof: step-level success does not re-evaluate the finding's rule",
                })
            }
        }
    };
    let (recheck_state, still_reproduces, recheck_reason) = match &recheck {
        None => (
            "no_surface",
            serde_json::Value::Null,
            "the finding's rule has no audit surface to re-check — verify by targeted observation of its evidence targets".to_string(),
        ),
        Some(r) if r["gated"].as_bool() == Some(true) => (
            "gated",
            serde_json::Value::Null,
            format!(
                "profile '{}' is beyond observational and allow_mutation was not set — re-send tui_workflow action=verify with allow_mutation=true to run the re-check",
                recheck_profile.clone().unwrap_or_default()
            ),
        ),
        Some(r) => {
            let refired = r["rule_refired"].as_bool();
            match refired {
                Some(true) => (
                    "refired",
                    json!(true),
                    "the finding's rule fired again on a fresh pass of its audit surface".to_string(),
                ),
                Some(false) => (
                    "clean",
                    json!(false),
                    format!(
                        "a fresh pass of profile '{}' produced no instance of this finding (no fingerprint or rule match)",
                        recheck_profile.clone().unwrap_or_default()
                    ),
                ),
                None => (
                    "indeterminate",
                    serde_json::Value::Null,
                    "the re-check ran but produced no rule-level answer".to_string(),
                ),
            }
        }
    };

    // Persist the verification record so a later caller can cite it.
    let record = crate::audit::verification::VerificationRecord {
        finding_fingerprint: fingerprint.clone(),
        finding_id: finding_id.clone(),
        session: p.id.clone().unwrap_or_else(|| "primary".to_string()),
        generation: recheck
            .as_ref()
            .and_then(|r| r["generation"].as_u64())
            .unwrap_or(0) as u32,
        strategy: strategy.matched_on.to_string(),
        replay: match &replay_result {
            None => "skipped".to_string(),
            Some(r) if r["steps_failed"].as_u64().unwrap_or(0) > 0 => "ran".to_string(),
            Some(_) => "ran".to_string(),
        },
        recheck: recheck_state.to_string(),
        verdict: match &still_reproduces {
            serde_json::Value::Bool(true) => "still_reproduces".to_string(),
            serde_json::Value::Bool(false) => "no_longer_reproduces".to_string(),
            _ => "undetermined".to_string(),
        },
        evidence_refs: {
            let mut refs = Vec::new();
            if let Some(r) = &replay_result {
                if let Some(id) = r["scenario_id"].as_str() {
                    refs.push(format!("scenario:{id}"));
                }
            }
            if let Some(name) = &recheck_profile {
                refs.push(format!("audit_pass:{}", name));
            }
            refs
        },
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    {
        let mut run = s.run.lock().unwrap();
        run.record_verification(record);
    }

    ok(json!({
        "workflow": "verify",
        "finding_id": finding_id,
        "finding_fingerprint": fingerprint,
        "strategy": {
            "matched_on": strategy.matched_on,
            "recheck_profile": recheck_profile,
            "needs_reproduction": strategy.needs_reproduction,
            "allow_mutation": allow_mutation,
        },
        "reproduction_setup": replay_result,
        "recheck": recheck,
        "verdict": {
            "still_reproduces": still_reproduces,
            "recheck_state": recheck_state,
            "reason": recheck_reason,
            "replay": replay_verdict,
        },
        "note": "this verification is recorded in the run's evidence — cite it via the finding's fingerprint",
    }))
}
