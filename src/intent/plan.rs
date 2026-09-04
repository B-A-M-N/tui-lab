//! Multi-step intent plans ([`plan_intent`]): focus-secured activation
//! sequences and the [`IntentPlan`]/[`PlannedStep`] records the MCP surface
//! shows the agent before anything is sent.
//!
//! Split from the former single-file `intent` (review §15 god-object
//! residue).

use crate::backend::MouseButton;
use crate::execution::CanonicalAction;
use crate::semantic::controls::Control;
use crate::semantic::SemanticScreen;

use super::vocab::{
    classify_risk, plan_action, resolve_target, ActionRisk, ActionTarget, ActionVerb, IntentError,
};

// ─── Multi-step intent plans (review §8) ──────────────────────────────────
//
// `plan_action` returns ONE `CanonicalAction`. For keypress verbs that is
// "press Enter wherever the app currently thinks focus is" — the motivating
// defect: resolve_intent could identify `button/save`, and the plan would
// still be a bare Enter that activates whatever is focused NOW. An honest
// activation plan must first SECURE focus on the resolved target and verify
// it landed there before the key is sent.

/// One step of an [`IntentPlan`]. `EnsureFocus`/`AssertFocus` bracket the
/// payload so an executor cannot send the key without the guard in between.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedStep {
    /// Move focus to `target_id` (mouse click at the control's center —
    /// the only verified direct route; keyboard traversal is NOT guessed).
    /// Skipped by the executor when the target is already focused.
    EnsureFocus {
        target_id: String,
        click: CanonicalAction,
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

/// Resolve + plan a focus-SECURED intent (review §8). Verbs whose payload
/// lands on the focused control (Activate/Select/Open/Toggle — the Enter
/// and Space keys) get `EnsureFocus → AssertFocus → Act`; direct verbs
/// (Click, Focus, Type-into) plan as a single `Act` step because their
/// action already names its own target.
pub fn plan_intent(
    sem: &SemanticScreen,
    target: &ActionTarget,
    verb: ActionVerb,
) -> Result<IntentPlan, IntentError> {
    let control = resolve_target(sem, target)?;
    let risk = classify_risk(&verb, Some(&control));
    let key_verbs = matches!(
        verb,
        ActionVerb::Activate | ActionVerb::Select | ActionVerb::Open | ActionVerb::Toggle
    );
    let steps = if key_verbs {
        if !control.focusable {
            return Err(IntentError::VerbMismatch {
                target: control.id.clone(),
                verb: verb.name().into(),
            });
        }
        let click = CanonicalAction::MouseClick {
            button: MouseButton::Left,
            x: control.bounds.x.saturating_add(control.bounds.width / 2),
            y: control.bounds.y,
        };
        vec![
            PlannedStep::EnsureFocus {
                target_id: control.id.clone(),
                click,
            },
            PlannedStep::AssertFocus {
                target_id: control.id.clone(),
            },
            PlannedStep::Act(plan_action(&verb, &control)?),
        ]
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

// ─── key serialization (the inverse of the MCP key parser) ─────────────────
