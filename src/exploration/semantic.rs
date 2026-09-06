//! Semantic (scripted) exploration (Wave D item 35).
//!
//! Random keyboard fuzz stays in [`super::random`]; this mode *reads the
//! screen* and drives what it sees: affordances with named keys, focusable
//! controls the interaction has not reached, and risk-gated activation.
//! Deterministic in outcome for a given app state: same screen → same plan.
//!
//! Every step goes through the one canonical executor, and the state graph
//! records what actually happened — same contract as the random mode.

use crate::exploration::candidates::CandidateContext;
use crate::exploration::random::ExplorationCompletionReason;
use crate::exploration::state_graph::{ExplorationBudget, StateGraph, StateIdentity};
use crate::intent::{resolve_target, ActionRisk, ActionTarget, ActionVerb};
use crate::session::state::Session;

/// One semantic exploration step.
#[derive(Debug, serde::Serialize)]
pub struct SemanticStep {
    /// What the explorer read that made it act ("affordance key 'q' for quit,
    /// never tried from this state").
    pub motive: String,
    /// The target that was acted on, when target-directed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The canonical action name executed.
    pub action: String,
    pub changed: bool,
    pub settle: crate::execution::SettleStatus,
    /// Whether the process was still running after the step.
    pub process_running: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct SemanticExploreReport {
    pub steps: Vec<SemanticStep>,
    pub completion_reason: ExplorationCompletionReason,
    /// Focus-graph edges accumulated during this exploration (stable IDs).
    pub focus_edges: usize,
    pub states_seen: usize,
    /// Item 29: per-dimension novelty scoreboard.
    pub novelty: crate::exploration::novelty::NoveltySummary,
    pub elapsed_ms: u64,
}

/// Run a semantic exploration: read the screen, pick the highest-value
/// evidential candidate, execute, repeat until the budget or the action
/// allowance runs out.
///
/// `max_risk` gates everything: an explorer at `safe` will move focus but
/// never activate anything (item 33).
pub fn run(
    session: &mut Session,
    graph: &mut StateGraph,
    focus_graph: &mut crate::semantic::focus_graph::FocusGraph,
    budget: &ExplorationBudget,
    max_actions: u32,
    max_risk: ActionRisk,
) -> anyhow::Result<SemanticExploreReport> {
    run_with_contract(
        session,
        graph,
        focus_graph,
        budget,
        max_actions,
        max_risk,
        None,
    )
}

/// Contract-armed variant (Wave E item 49): the loaded project contract
/// feeds the candidate context, so declared-but-unexercised keys join the
/// exploration queue with `contract` evidence.
#[allow(clippy::too_many_arguments)]
pub fn run_with_contract(
    session: &mut Session,
    graph: &mut StateGraph,
    focus_graph: &mut crate::semantic::focus_graph::FocusGraph,
    budget: &ExplorationBudget,
    max_actions: u32,
    max_risk: ActionRisk,
    contract: Option<&crate::design::ProjectContract>,
) -> anyhow::Result<SemanticExploreReport> {
    run_evidenced(
        session,
        graph,
        focus_graph,
        budget,
        max_actions,
        max_risk,
        contract,
        None,
    )
}

/// Ledger-evidenced variant (audit finding 22): when `run` is given, every
/// executed action ALSO enters the run's canonical transaction ledger and
/// frame store — one history, not two. The exploration metadata stays on
/// the exploration report; the ledger carries the exact canonical
/// transaction (linked with `exploration:<seq>` provenance), so "which
/// exploration step produced frame:N?" is answerable from the run record.
#[allow(clippy::too_many_arguments)]
pub fn run_evidenced(
    session: &mut Session,
    graph: &mut StateGraph,
    focus_graph: &mut crate::semantic::focus_graph::FocusGraph,
    budget: &ExplorationBudget,
    max_actions: u32,
    max_risk: ActionRisk,
    contract: Option<&crate::design::ProjectContract>,
    mut run: Option<&mut crate::run::RunContext>,
) -> anyhow::Result<SemanticExploreReport> {
    let started = std::time::Instant::now();
    let mut steps: Vec<SemanticStep> = Vec::new();
    let mut identities: Vec<StateIdentity> = Vec::new();
    let mut reason = ExplorationCompletionReason::CleanExit;
    // Item 29: multi-dimensional novelty scoreboard.
    let mut novelty_ledger = crate::exploration::novelty::NoveltyLedger::new();

    for seq in 0..max_actions {
        if started.elapsed().as_millis() as u64 >= budget.max_runtime_ms {
            reason = ExplorationCompletionReason::TimeBudget;
            break;
        }
        if identities.len() as u32 >= budget.max_unique_states {
            reason = ExplorationCompletionReason::UniqueStateBudget;
            break;
        }
        if steps.len() as u32 >= budget.max_depth {
            reason = ExplorationCompletionReason::DepthBudget;
            break;
        }

        // Fused truth (re-review Wave-2 item 16).
        let (screen, sem, _, _) = session.observe_fused(40)?;
        let identity = StateIdentity::with_semantic(&screen, &sem);
        if !identities.contains(&identity) {
            identities.push(identity.clone());
        }
        let current = identity.id();

        // Coverage: controls this run's interaction has reached (focus
        // graph nodes are exactly those).
        let coverage: Vec<String> = focus_graph.nodes.keys().cloned().collect();
        let history: Vec<String> = steps.iter().map(|s| s.action.clone()).collect();

        let ctx = CandidateContext {
            state_graph: graph,
            current: current.clone(),
            action_history: &history,
            coverage: &coverage,
            contract,
            allowed_risk: max_risk,
        };
        let candidates = crate::exploration::candidates::suggest(&screen, &sem, &ctx);
        let Some(candidate) = candidates.first() else {
            // Nothing evidential left to do from this state.
            reason = ExplorationCompletionReason::CleanExit;
            break;
        };

        // Resolve the concrete action: target-directed candidates go
        // through intent resolution (ambiguous targets are skipped, never
        // first-matched); plain key candidates execute directly.
        let (action, target_desc, motive) = build_action(candidate, &sem);
        let Some(action) = action else {
            // Unresolvable target: record and stop rather than spin.
            reason = ExplorationCompletionReason::CleanExit;
            steps.push(SemanticStep {
                motive: format!("candidate unresolvable: {motive}"),
                target: target_desc,
                action: "none".into(),
                changed: false,
                settle: crate::execution::SettleStatus::Skipped,
                process_running: screen.process.running,
            });
            break;
        };

        // Execute via the one canonical executor.
        let tx = crate::execution::execute_act_as(
            session,
            crate::execution::DriveOrigin::ExploreSemantic,
            &action,
            80,
            900,
            false,
        )?;
        // Audit finding 23: the action's IDENTITY is the exact canonical
        // signature (`mouse:left:click@12,3`), not the generic kind name —
        // distinct keys/targets must not collapse in the graph, history,
        // or novelty dimensions. Finding 22 + beta-audit P0-7: the
        // transaction enters the canonical ledger through the session's
        // installed evidence sink (committed inside execute_act_as above);
        // the `run` reference stays only as the legacy route for callers
        // without authorized dispatch (tests).
        let action_sig = action.signature();
        if session.evidence_sink().is_none() {
            if let Some(run) = run.as_deref_mut() {
                let _ = run.record_interaction(&session.id, &tx);
            }
        }
        let after = tx.after().clone();
        // Re-review P0.7: the after-state identity comes from the SAME
        // fused authority as the before-state — the state graph never
        // mixes fused-before with inference-after (which could see one
        // action as a transition between states that native semantics
        // say are identical, or miss one that native semantics
        // distinguish).
        let after_sem = session.fuse_screen(&after);
        let after_identity = StateIdentity::with_semantic(&after, &after_sem);

        graph.record_transition_identity(&identity, &after_identity, &action_sig);
        // Focus edges with the action as provenance.
        if let (Some(f), Some(t)) = (
            sem.focus.control_id.as_deref(),
            after_sem.focus.control_id.as_deref(),
        ) {
            focus_graph.transition(f, t, after_sem.focus.control.as_deref(), &action_sig);
        }

        // Item 29: feed the novelty ledger from what this step evidenced.
        {
            let control_ids: Vec<String> =
                after_sem.controls.iter().map(|c| c.id.clone()).collect();
            let focus_edge_list: Vec<(String, String, String)> = match (
                sem.focus.control_id.as_ref(),
                after_sem.focus.control_id.as_ref(),
            ) {
                (Some(f), Some(t)) => {
                    vec![(f.clone(), t.clone(), action_sig.clone())]
                }
                _ => Vec::new(),
            };
            let coverage_targets = session.native_coverage_targets();
            let mode_states = super::random::current_mode_states_pub(session);
            novelty_ledger.note(
                Some(&after_identity),
                &focus_edge_list,
                &control_ids,
                &coverage_targets,
                &mode_states,
            );
        }

        let changed = identity.id() != after_identity.id();
        steps.push(SemanticStep {
            motive,
            target: target_desc,
            action: action_sig.clone(),
            changed,
            settle: tx.settle,
            process_running: after.process.running,
        });

        if !after.process.running {
            reason = ExplorationCompletionReason::CleanExit;
            break;
        }
        let _ = seq;
    }

    if steps.len() as u32 >= max_actions && reason == ExplorationCompletionReason::CleanExit {
        reason = ExplorationCompletionReason::ActionBudget;
    }

    Ok(SemanticExploreReport {
        steps,
        completion_reason: reason,
        focus_edges: focus_graph.edges.len(),
        states_seen: identities.len(),
        novelty: novelty_ledger.summary(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Turn a candidate into a concrete [`crate::execution::CanonicalAction`].
/// Returns `None` when the target cannot resolve to exactly one control.
fn build_action(
    candidate: &crate::exploration::candidates::Candidate,
    sem: &crate::semantic::SemanticScreen,
) -> (
    Option<crate::execution::CanonicalAction>,
    Option<String>,
    String,
) {
    use crate::execution::CanonicalAction as CA;
    let reason_text: String = candidate
        .reasons
        .iter()
        .map(|r| format!("[{}] {}", r.source, r.claim))
        .collect::<Vec<_>>()
        .join("; ");

    // Target-directed candidate: resolve through the intent layer.
    if candidate.action["action"] == "focus_target" {
        let Some(id) = candidate.action["target"]["id"].as_str() else {
            return (None, None, reason_text);
        };
        match resolve_target(sem, &ActionTarget::Id { id: id.to_string() }) {
            Ok(control) => {
                // Audit P0-4: this IS a click — on a button it activates.
                // The candidate's risk class now says Mutating (the gate
                // keeps it out of safe-only runs); the motive records the
                // mechanism honestly instead of dressing a click up as
                // "focus".
                let mut motive = format!(
                    "mouse click at control '{}' (click-focus: the click can \
                     activate the control — this candidate is risk-classed \
                     mutating)",
                    control.id
                );
                if !reason_text.is_empty() {
                    motive.push_str("; ");
                    motive.push_str(&reason_text);
                }
                match crate::intent::plan_action(&ActionVerb::Click, &control) {
                    Ok(a) => (Some(a), Some(control.id.clone()), motive),
                    Err(e) => (None, Some(control.id.clone()), e.message()),
                }
            }
            Err(e) => (None, Some(id.to_string()), e.message()),
        }
    } else if let Some(key) = candidate.action["key"].as_str() {
        match crate::mcp::helpers::parse_key_public(key) {
            Ok(k) => (
                Some(CA::Key { key: k }),
                candidate.control_id.clone(),
                reason_text,
            ),
            Err(_) => (
                None,
                candidate.control_id.clone(),
                format!("unparseable key '{key}'"),
            ),
        }
    } else {
        (None, candidate.control_id.clone(), reason_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// build_action resolves a target candidate against the semantic screen;
    /// an ID that matches nothing yields None with the error as motive —
    /// never a first-match guess.
    #[test]
    fn target_candidates_resolve_or_fail_honestly() {
        let screen = crate::screen::ScreenState::new(80, 24);
        let sem = crate::semantic::analyze(&screen);
        let candidate = crate::exploration::candidates::Candidate {
            action: serde_json::json!({
                "action": "focus_target",
                "target": { "by": "id", "id": "button/nope" }
            }),
            reasons: vec![],
            risk: ActionRisk::Safe,
            control_id: Some("button/nope".into()),
        };
        let (action, target, motive) = build_action(&candidate, &sem);
        assert!(
            action.is_none(),
            "unresolvable target must not fabricate an action"
        );
        assert_eq!(target.as_deref(), Some("button/nope"));
        assert!(motive.contains("no control matches"), "{motive}");
    }
}
