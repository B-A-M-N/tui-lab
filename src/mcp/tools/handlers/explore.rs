//! tui_explore: random/semantic exploration and graphs.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_explore` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_explore(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiExploreParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::ExploreMode as EM;
    let mode_sel = p.mode.clone();
    let Some(emode) = (match &mode_sel {
        crate::mcp::params::Known::Known(m) => Some(*m),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown explore mode '{}' (expected one of: {})",
                    o,
                    <EM as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    // StateGraph needs no session at all; every other mode drives the
    // session through its actor with run locks taken on the actor thread.
    if emode == EM::StateGraph {
        let run = s.run.lock().unwrap();
        return ok(json!({
            "states": run.graphs().state_graph.state_count(),
            "transitions": run.graphs().state_graph.transition_count(),
            "dead_ends": run.graphs().state_graph
                .find_dead_ends()
                .into_iter()
                .map(|n| n.structure_hash.clone())
                .collect::<Vec<String>>(),
            "edges": run.graphs().state_graph.edge_list(),
            "visit_counts": run.graphs().state_graph.visit_counts(),
            "budget_exhausted": run.graphs().state_graph.budget_exhausted(),
            // The ID-keyed focus graph (Wave D item 36): Tab order
            // and reverse-traversal proof, accumulated across every
            // observe and audit in this run.
            "focus_graph": run.graphs().focus_graph.summary(),
        }));
    }
    let selector = p.id.clone();
    let run = s.run.clone();
    let explore = p.clone();
    let server = s.clone();
    // Audit P0-5 (second half): the typed selector must also REFUSE the
    // unknown arm. An unrecognized `max_risk` used to fall through
    // `known()` → default, which for a SAFETY selector means a typo could
    // change what the explorer was allowed to do (the old free-string
    // fallback even widened it to Mutating). A safety knob is not
    // forward-compatible by nature — refuse and name the accepted values.
    let max_risk_allowed = match p.max_risk.as_ref() {
        None => crate::intent::ActionRisk::Safe,
        Some(crate::mcp::params::Known::Known(r)) => r.to_risk(),
        Some(crate::mcp::params::Known::Other(o)) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown max_risk '{}' (expected one of: {})",
                    o,
                    <crate::mcp::params::ExploreRisk as crate::mcp::params::EnumVariants>::VARIANTS
                        .join(", ")
                ),
            );
        }
    };
    // Wave G item 76: random/semantic exploration drives the app, so the
    // human control lease blocks them. guided_candidates and state_graph
    // only read state and stay allowed — they were filtered above (the
    // StateGraph early-return) and need no session at all.
    let lease_gates_driving = matches!(emode, EM::Random | EM::Semantic);
    s.with_sess(selector.as_deref(), move |sess| {
        let p = explore;
        if lease_gates_driving {
            if let Some(refused) = lease_refused(sess) {
                return refused;
            }
        }
        match emode {
            EM::GuidedCandidates => {
                // Novel action candidates for Hermes to choose (spec 4.3),
                // Wave D item 34: every reason is evidential. The candidate
                // context carries the run's state graph, the current state's
                // layered identity, the actions actually executed this run,
                // the coverage set, and the risk allowance.
                let (screen, sem, _, _) = match sess.observe_fused(40) {
                    Ok(t) => t,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                let identity =
                    crate::exploration::state_graph::StateIdentity::with_semantic(&screen, &sem);
                let current = identity.id();
                let candidate_list = {
                    let run = run.lock().unwrap();
                    let action_history: Vec<String> = run
                        .transactions()
                        .iter()
                        .map(|t| t.action.clone())
                        .collect();
                    let coverage: Vec<String> =
                        run.graphs().focus_graph.nodes.keys().cloned().collect();
                    // Item 49: a loaded contract feeds the `contract` evidence
                    // source — declared keys never exercised become candidates
                    // citing the contract, not guesses.
                    let contract = run.contract();
                    let ctx = crate::exploration::candidates::CandidateContext {
                        state_graph: &run.graphs().state_graph,
                        current,
                        action_history: &action_history,
                        coverage: &coverage,
                        contract,
                        // Audit P0-5: typed selector; the default is SAFE —
                        // mutation is explicit, and the unknown arm was
                        // refused before the actor boundary.
                        allowed_risk: max_risk_allowed,
                    };
                    crate::exploration::candidates::suggest(&screen, &sem, &ctx)
                };
                ok(json!({
                    "novel_actions": candidate_list,
                    "state": identity.id().as_str(),
                }))
            }
            EM::Random => {
                let seed = p.seed.unwrap_or(4242);
                let recording_path = p.recording_path.as_deref().map(std::path::Path::new);
                if recording_path.is_some() {
                    sess.enable_recording(false);
                }
                // The budget is the authority (re-review item 13): limits come
                // from the run's ExplorationBudget, with an optional action
                // override; the report names the real completion reason.
                // Items 27/28: the risk allowance gates the pool — the
                // default (mutating) keeps Escape (unknown) out.
                let budget = {
                    let run = run.lock().unwrap();
                    // Audit P0-5: typed selector, safe default (see
                    // guided_candidates above).
                    let allowed_risk = p
                        .max_risk
                        .as_ref()
                        .and_then(|r| r.known().copied())
                        .map(|r| r.to_risk())
                        .unwrap_or(crate::intent::ActionRisk::Safe);
                    crate::exploration::random::Budget {
                        max_actions: p
                            .actions
                            .unwrap_or(run.graphs().state_graph.budget().max_actions),
                        allowed_risk,
                        ..crate::exploration::random::Budget::from_graph_budget(
                            run.graphs().state_graph.budget(),
                        )
                    }
                };
                // Audit finding 22: the explorer's transactions enter the
                // canonical run ledger — the graph summary below stays the
                // exploration view, the ledger the evidence view.
                let mut run_guard = run.lock().unwrap();
                match crate::exploration::random::run_evidenced(
                    sess,
                    seed,
                    budget,
                    recording_path,
                    Some(&mut run_guard),
                ) {
                    Ok(report) => {
                        // The state graph records WHAT ACTUALLY HAPPENED
                        // (re-review item 12): transitions come from the
                        // ordered ExplorationStep records (before → after via
                        // the real action), not from post-hoc hash lists.
                        let graph_summary = {
                            crate::exploration::random::record_steps(
                                &mut run_guard.graphs_mut().state_graph,
                                &report.steps,
                            );
                            json!({
                                "states": run_guard.graphs().state_graph.state_count(),
                                "transitions": run_guard.graphs().state_graph.transition_count(),
                                "dead_ends": run_guard.graphs().state_graph.find_dead_ends().len(),
                            })
                        };
                        // RELEASE run_guard BEFORE taking the run lock again:
                        // the persist block below re-locks `run`, and
                        // minimize_crash_finding locks `self.run` internally.
                        // Holding a std::sync::MutexGuard across either (both
                        // non-reentrant) self-deadlocks the server — the map
                        // is a value now; nothing below needs the guard.
                        drop(run_guard);
                        // Persist the graph when the run is persistent.
                        let graph_path = {
                            let run = run.lock().unwrap();
                            match run.run_dir() {
                                Some(dir) => {
                                    let path = dir.join("state_graph.json");
                                    let payload = json!({
                                        "edges": run.graphs().state_graph.edge_list(),
                                        "visit_counts": run.graphs().state_graph.visit_counts(),
                                        "known_states": run.graphs().state_graph.known_states(),
                                    });
                                    std::fs::write(&path, payload.to_string())
                                        .ok()
                                        .map(|_| path.to_string_lossy().to_string())
                                }
                                None => None,
                            }
                        };
                        // Wave D item 38: a crash exit gets the full
                        // minimization pipeline — clean restart, delta-debug
                        // replay, saved Scenario, Finding with
                        // reproduction=scenario_id.
                        let repro = server.minimize_crash_finding(sess, seed, &report);
                        ok(json!({
                            "seed": seed,
                            "report": report,
                            "state_graph": graph_summary,
                            "state_graph_path": graph_path,
                            "reproduction": repro,
                        }))
                    }
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            EM::Semantic => {
                // Wave D item 35: screen-reading exploration. Picks the
                // top evidential candidate each round (affordances, untried
                // keys from the graph, unreached controls) and executes it
                // through the canonical executor. Focus edges land in the
                // run's ID-keyed FocusGraph with the acting key as via.
                // Audit P0-5: typed selector, safe default (see
                // guided_candidates above); the unknown arm was refused
                // before the actor boundary.
                let max_risk = max_risk_allowed;
                let (graph_budget, run_graph_summary, contract) = {
                    let run = run.lock().unwrap();
                    (
                        run.graphs().state_graph.budget().clone(),
                        (
                            run.graphs().state_graph.state_count(),
                            run.graphs().state_graph.transition_count(),
                        ),
                        run.contract().cloned(),
                    )
                };
                let max_actions = p.actions.unwrap_or(20);
                // Local graphs during the loop (session I/O must not hold
                // the run lock); merged into the run after.
                let mut local_graph =
                    crate::exploration::state_graph::StateGraph::new(graph_budget.clone());
                let mut focus_graph = crate::semantic::focus_graph::FocusGraph::new();
                // Audit finding 22: the run ledger rides INTO the explorer
                // so each executed action's transaction lands in the
                // canonical evidence store as it happens, not as a summary.
                let mut run_guard = run.lock().unwrap();
                let report = match crate::exploration::semantic::run_evidenced(
                    sess,
                    &mut local_graph,
                    &mut focus_graph,
                    &graph_budget,
                    max_actions,
                    max_risk,
                    contract.as_ref(),
                    Some(&mut run_guard),
                ) {
                    Ok(r) => r,
                    Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
                };
                // Merge what actually happened into the run's graphs.
                {
                    run_guard.graphs_mut().state_graph.merge(&local_graph);
                    run_guard.graphs_mut().focus_graph.merge(&focus_graph);
                }
                ok(json!({
                    "mode": "semantic",
                    "max_risk": max_risk.name(),
                    "report": report,
                    "focus_graph": focus_graph.summary(),
                    "run_graph_before": {
                        "states": run_graph_summary.0,
                        "transitions": run_graph_summary.1,
                    },
                }))
            }
            EM::StateGraph => unreachable!("handled above the actor boundary"),
        }
    })
    .await
    .unwrap_or_else(|e| e)
}
