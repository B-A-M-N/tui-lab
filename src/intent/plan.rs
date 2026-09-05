//! Multi-step intent plans ([`plan_intent`]): focus-secured activation
//! sequences and the [`IntentPlan`]/[`PlannedStep`] records the MCP surface
//! shows the agent before anything is sent.
//!
//! Split from the former single-file `intent` (review §15 god-object
//! residue).
//!
//! Finding 3 (audit): the focus-securing step must NEVER use an activating
//! primitive. The old plan clicked the target to move focus — but a click
//! on a button/menu item is itself an activation, so Activate/Toggle plans
//! could fire the payload TWICE (click activates, then the Enter/Space
//! payload activates again). The focus move is now a *verified Tab
//! traversal* built from the run's FocusGraph: press Tab (or Shift+Tab)
//! along PROVEN edges until an AssertFocus guard confirms the target holds
//! focus, then send the payload. Without a proven route the plan is
//! `Unsupported` — never a guessed click.

use crate::backend::{KeyCode, KeyEvent, KeyModifiers};
use crate::execution::CanonicalAction;
use crate::semantic::controls::Control;
use crate::semantic::focus_graph::FocusGraph;
use crate::semantic::SemanticScreen;

use super::vocab::{
    classify_risk, plan_action, resolve_target, ActionRisk, ActionTarget, ActionVerb, IntentError,
};

/// How many traversal hops a focus route may take before we refuse. A
/// FocusGraph cycle can be arbitrarily long; an honest plan over a real TUI
/// reaches any focusable control within a screenful of tabs.
const MAX_FOCUS_HOPS: usize = 32;

/// One step of an [`IntentPlan`]. `MoveFocus`/`AssertFocus` bracket the
/// payload so an executor cannot send the key without the guard in between.
/// `MoveFocus` carries NON-activating keys only (Tab / Shift+Tab) — finding
/// 3B: no click can appear here.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedStep {
    /// Advance focus toward `target_id` by pressing the traversal key the
    /// FocusGraph proved (`tab` or `shift+tab`). Skipped by the executor
    /// when the target already holds focus.
    MoveFocus {
        target_id: String,
        key: CanonicalAction,
    },
    /// Stale-state guard: re-observe and refuse unless focus == target_id.
    /// Runs BETWEEN the focus move and the key, so a race or a focus trap
    /// stops the plan before it activates the wrong control.
    AssertFocus { target_id: String },
    /// The payload action (the Enter/Space/Type the verb asked for).
    Act(CanonicalAction),
}

/// A multi-step intent: focus-secured, guarded, then executed. The
/// executor runs steps in order and aborts on the first failure.
#[derive(Debug, Clone)]
pub struct IntentPlan {
    /// The control the target resolved to (at plan time).
    pub control: Control,
    /// The verb that will be applied.
    pub verb: ActionVerb,
    /// Risk class (verb base risk, raised by target evidence).
    pub risk: ActionRisk,
    /// Steps in execution order.
    pub steps: Vec<PlannedStep>,
}

/// Resolve + plan a focus-SECURED intent (review §8, finding 3). Verbs
/// whose payload lands on the focused control (Activate/Select/Open/Toggle
/// — the Enter and Space keys) get `MoveFocus… → AssertFocus → Act`; the
/// focus route is a chain of PROVEN Tab/Shift+Tab edges from the
/// FocusGraph, one `MoveFocus` step per hop, each guarded by the trailing
/// `AssertFocus` before the payload. Direct verbs (Click, Type-into) plan
/// as a single `Act` step because their action already names its own
/// target. `Focus` plans as traversal-only (`MoveFocus… → AssertFocus`)
/// with no payload — the non-activating focus movement the verb name
/// always promised.
pub fn plan_intent(
    sem: &SemanticScreen,
    target: &ActionTarget,
    verb: ActionVerb,
) -> Result<IntentPlan, IntentError> {
    plan_intent_with_graph(sem, target, verb, &FocusGraph::new())
}

/// [`plan_intent`] with a traversal graph. The FocusGraph carries PROVEN
/// focus edges (`via=tab` / `via=shift+tab`, recorded by real observed
/// transitions — exploration, the keyboard audit, prior intent
/// executions); planning from an empty graph honestly refuses rather than
/// guessing. The run layer passes the live graph so every executed intent
/// enriches the next plan.
pub fn plan_intent_with_graph(
    sem: &SemanticScreen,
    target: &ActionTarget,
    verb: ActionVerb,
    graph: &FocusGraph,
) -> Result<IntentPlan, IntentError> {
    let control = resolve_target(sem, target)?;
    let risk = classify_risk(&verb, Some(&control));
    // Verbs whose payload lands on the FOCUSED control — focus must be
    // secured first. (Click/Type name their own target; Focus secures and
    // stops.)
    let key_verbs = matches!(
        verb,
        ActionVerb::Activate | ActionVerb::Select | ActionVerb::Open | ActionVerb::Toggle
    );
    let needs_focus = key_verbs || matches!(verb, ActionVerb::Focus);
    let steps = if needs_focus {
        if !control.focusable {
            return Err(IntentError::VerbMismatch {
                target: control.id.clone(),
                verb: verb.name().into(),
            });
        }
        let mut steps = focus_route(sem, &control, graph)?;
        if key_verbs {
            steps.push(PlannedStep::Act(plan_action(&verb, &control)?));
        }
        steps
    } else {
        vec![PlannedStep::Act(plan_action(&verb, &control)?)]
    };
    Ok(IntentPlan {
        control,
        verb,
        risk,
        steps,
    })
}

/// Build the non-activating focus route to `control`: a chain of
/// `MoveFocus` hops over PROVEN Tab/Shift+Tab edges ending in the
/// `AssertFocus` guard. The route is recomputed hop-by-hop shape (the
/// executor re-observes between hops) but planned from the graph's proven
/// edges; no route = `Unsupported`, never a click.
fn focus_route(
    sem: &SemanticScreen,
    control: &Control,
    graph: &FocusGraph,
) -> Result<Vec<PlannedStep>, IntentError> {
    let target_id = control.id.clone();
    // Already focused: no hops — just the guard, which re-verifies against
    // the LIVE frame at execution time.
    if sem.focus.control_id.as_deref() == Some(target_id.as_str()) {
        return Ok(vec![PlannedStep::AssertFocus {
            target_id: target_id.clone(),
        }]);
    }
    let Some(from) = sem.focus.control_id.clone() else {
        return Err(IntentError::Unsupported {
            target: target_id,
            verb: "focus-route".into(),
            why: "no focus is currently established (nothing focused), so there is no \
                  proven Tab route to the target; Tab from an unknown origin could land \
                  the payload on the wrong control. Focus something first (an explicit \
                  Click, or run the keyboard audit to prove the Tab order), then retry"
                .into(),
        });
    };
    let hops = route_hops(graph, &from, &target_id).ok_or_else(|| IntentError::Unsupported {
        target: target_id.clone(),
        verb: "focus-route".into(),
        why: format!(
            "no proven Tab route from focus '{}' to '{}' in the FocusGraph \
             (focus edges are recorded by real observed transitions — exploration, \
             the keyboard audit, prior intents). Run the keyboard audit or an \
             exploration pass to prove the traversal order, then retry; a click was \
             NOT substituted because clicking is an activation",
            from, target_id
        ),
    })?;
    if hops.len() > MAX_FOCUS_HOPS {
        return Err(IntentError::Unsupported {
            target: target_id,
            verb: "focus-route".into(),
            why: format!(
                "the proven Tab route is {} hops (max {}); refusing to spam-traverse",
                hops.len(),
                MAX_FOCUS_HOPS
            ),
        });
    }
    let mut steps: Vec<PlannedStep> = hops
        .into_iter()
        .map(|key| PlannedStep::MoveFocus {
            target_id: target_id.clone(),
            key,
        })
        .collect();
    // The final guard: focus must be ON the target before any payload.
    steps.push(PlannedStep::AssertFocus {
        target_id: target_id.clone(),
    });
    Ok(steps)
}

/// Walk the FocusGraph's PROVEN Tab edges from `from` toward `to`.
/// `Some(keys)` is the ordered key sequence (Tab / Shift+Tab actions);
/// `None` when no proven route exists. BFS over the edge set — edges are
/// directed and carry their own via-key, so the walk never guesses.
fn route_hops(graph: &FocusGraph, from: &str, to: &str) -> Option<Vec<CanonicalAction>> {
    use std::collections::{HashMap, VecDeque};
    let mut queue = VecDeque::new();
    let mut prev: HashMap<String, (String, CanonicalAction)> = HashMap::new();
    queue.push_back(from.to_string());
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(from.to_string());
    while let Some(cur) = queue.pop_front() {
        if cur == to {
            // Reconstruct the key sequence.
            let mut keys = Vec::new();
            let mut node = cur.clone();
            while node != from {
                let (p, k) = prev.get(&node)?.clone();
                keys.push(k);
                node = p;
            }
            keys.reverse();
            return Some(keys);
        }
        // Only PROVEN traversal edges (tab / shift+tab) are legal hops —
        // activating keys (Enter) and unproven routes never qualify.
        for edge in graph.traversal_successors(&cur) {
            let key = match edge.via.as_str() {
                "tab" => CanonicalAction::Key {
                    key: KeyEvent::new(KeyCode::Tab),
                },
                "shift+tab" => CanonicalAction::Key {
                    key: KeyEvent::with_modifiers(KeyCode::Tab, KeyModifiers::SHIFT),
                },
                _ => unreachable!("traversal_successors filters non-traversal vias"),
            };
            if visited.insert(edge.to.clone()) {
                prev.insert(edge.to.clone(), (cur.clone(), key));
                queue.push_back(edge.to.clone());
            }
        }
    }
    None
}

// ─── key serialization (the inverse of the MCP key parser) ─────────────────
