//! Agent intent layer (Wave D items 31–33): actions that target *things*,
//! not coordinates.
//!
//! A raw `mouse_click x=37 y=12` is blind: the agent had to read pixels, and
//! nothing stands between it and the wrong cell. An [`ActionTarget`] is the
//! alternative — `"activate #button/save"` — resolved against the live
//! semantic screen *at execution time*. Resolution is evidence-based:
//!
//! * a target that matches several controls is an [`IntentError::Ambiguous`],
//!   never a silent first-match (the classic `OK`/`Cancel` mis-click);
//! * a target that matches nothing is [`IntentError::NotFound`], with the
//!   nearest candidates in the error so the agent can self-correct;
//! * every resolved action carries an [`ActionRisk`] class, so an explorer
//!   can never treat "Next" and "Delete Database" the same way.
//!
//! The resolved form is a plain [`CanonicalAction`], so targeting composes
//! with the one canonical executor — settle waits, ledger, scenario capture —
//! without a second execution path.

mod keys;
mod plan;
mod vocab;

pub use plan::{plan_intent, IntentPlan, PlannedStep};
pub use vocab::{
    classify_risk, plan_action, resolve_intent, resolve_target, ActionRisk, ActionTarget,
    ActionVerb, ControlSummary, IntentError, ResolvedIntent,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent, KeyModifiers};
    use crate::execution::CanonicalAction;
    use crate::semantic::controls::{Control, ControlBounds, ControlKind};
    use crate::semantic::focus::FocusInfo;
    use crate::semantic::SemanticScreen;

    fn control(id: &str, kind: ControlKind, label: &str, x: u16, y: u16) -> Control {
        Control {
            id: id.into(),
            kind,
            label: label.into(),
            value: None,
            bounds: ControlBounds {
                x,
                y,
                width: 6,
                height: 1,
            },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
            evidence: Vec::new(),
            source: "inferred".into(),
        }
    }

    fn sem(controls: Vec<Control>, focused: Option<&str>) -> SemanticScreen {
        SemanticScreen {
            cols: 80,
            rows: 24,
            regions: Vec::new(),
            controls,
            focus: FocusInfo {
                control: None,
                control_id: focused.map(str::to_string),
                confidence: 0.9,
                evidence: Vec::new(),
            },
            relationships: Vec::new(),
            affordances: Vec::new(),
            components: Vec::new(),
        }
    }

    /// Item 31, core contract: "activate #save" resolves by ID to exactly
    /// one control and plans an Enter keypress.
    #[test]
    fn resolve_by_id_and_plan_activation() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            None,
        );
        let intent = resolve_intent(
            &s,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Activate,
        )
        .expect("resolve");
        assert_eq!(intent.control.id, "button/save");
        assert!(matches!(intent.action, CanonicalAction::Key { .. }));
        assert_eq!(intent.risk, ActionRisk::Mutating);
    }

    /// Review §8, the motivating defect: activation must activate the
    /// RESOLVED target, not wherever focus currently is. The plan is
    /// EnsureFocus (click on the target) → AssertFocus (guard) → Enter —
    /// never a bare Enter over the current focus.
    #[test]
    fn activation_plan_secures_focus_before_the_key() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            None, // NO current focus — the old test accepted a bare Enter here
        );
        let plan = plan_intent(
            &s,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Activate,
        )
        .expect("plan");
        assert_eq!(
            plan.steps.len(),
            3,
            "secure → guard → act: {:?}",
            plan.steps
        );
        match &plan.steps[0] {
            PlannedStep::EnsureFocus { target_id, click } => {
                assert_eq!(target_id, "button/save");
                assert!(
                    matches!(click, CanonicalAction::MouseClick { .. }),
                    "focus is secured by a click on the target: {click:?}"
                );
            }
            other => panic!("step 0 must EnsureFocus, got {other:?}"),
        }
        assert_eq!(
            plan.steps[1],
            PlannedStep::AssertFocus {
                target_id: "button/save".into()
            }
        );
        assert!(
            matches!(
                &plan.steps[2],
                PlannedStep::Act(CanonicalAction::Key { .. })
            ),
            "payload is the Enter: {:?}",
            plan.steps[2]
        );
    }

    /// Direct verbs (Click) need no focus securing: their action names the
    /// target by coordinates already. One step.
    #[test]
    fn click_plans_as_a_single_direct_step() {
        let s = sem(
            vec![control("button/ok", ControlKind::Button, "OK", 0, 5)],
            None,
        );
        let plan = plan_intent(
            &s,
            &ActionTarget::Id {
                id: "button/ok".into(),
            },
            ActionVerb::Click,
        )
        .expect("plan");
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(
            &plan.steps[0],
            PlannedStep::Act(CanonicalAction::MouseClick { .. })
        ));
    }

    /// Toggle on a non-focusable checkbox is refused at PLAN time — the
    /// focus-secured route cannot reach it, and a bare Space would toggle
    /// whatever IS focused.
    #[test]
    fn toggle_plan_refuses_unfocusable_target() {
        let mut c = control("check/archive", ControlKind::Checkbox, "Archive", 0, 5);
        c.focusable = false;
        let s = sem(vec![c], None);
        let err = plan_intent(
            &s,
            &ActionTarget::Id {
                id: "check/archive".into(),
            },
            ActionVerb::Toggle,
        )
        .expect_err("unfocusable toggle must refuse");
        assert!(matches!(err, IntentError::VerbMismatch { .. }), "{err:?}");
    }

    /// The motivating failure mode: two "Save" buttons must be an
    /// `ambiguous_target` error carrying both, never a first-match pick.
    #[test]
    fn ambiguous_text_target_is_an_error_with_matches() {
        let s = sem(
            vec![
                control("dialog/a/button/save", ControlKind::Button, "Save", 0, 1),
                control("dialog/b/button/save", ControlKind::Button, "Save", 0, 9),
            ],
            None,
        );
        let err = resolve_target(
            &s,
            &ActionTarget::Text {
                text: "Save".into(),
            },
        )
        .expect_err("must be ambiguous");
        let msg = err.message();
        match &err {
            IntentError::Ambiguous { matches, .. } => {
                assert_eq!(matches.len(), 2, "both matches reported");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert!(msg.contains("ambiguous"), "{msg}");
    }

    /// Not-found errors carry near candidates so the agent self-corrects
    /// without another observe round-trip.
    #[test]
    fn not_found_carries_candidates() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 1),
                control("button/next", ControlKind::Button, "Next", 10, 1),
            ],
            None,
        );
        // "Sav" is a substring of "Save" only → resolves unambiguously.
        let sav = resolve_target(&s, &ActionTarget::Text { text: "Sav".into() })
            .expect("unique substring resolves");
        assert_eq!(sav.id, "button/save");
        let err2 = resolve_target(
            &s,
            &ActionTarget::Text {
                text: "Cancle".into(),
            },
        )
        .expect_err("typo must not resolve");
        match &err2 {
            IntentError::NotFound { candidates, .. } => {
                assert!(
                    candidates.iter().any(|c| c.id == "button/save"),
                    "candidates: {candidates:?}"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Role + text resolution and focus-target resolution.
    #[test]
    fn role_and_focused_targets() {
        let s = sem(
            vec![
                control("field/host", ControlKind::Field, "Host", 0, 1),
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        // Role + text: unambiguous.
        let c = resolve_target(
            &s,
            &ActionTarget::Role {
                role: "button".into(),
                text: Some("Cancel".into()),
            },
        )
        .expect("role+text");
        assert_eq!(c.id, "button/cancel");
        // Role alone: three matches (field excluded) — ambiguous.
        assert!(matches!(
            resolve_target(
                &s,
                &ActionTarget::Role {
                    role: "button".into(),
                    text: None
                }
            ),
            Err(IntentError::Ambiguous { .. })
        ));
        // Focused target.
        let f = resolve_target(&s, &ActionTarget::Focused).expect("focused");
        assert_eq!(f.id, "button/cancel");
    }

    /// Item 33: risk classification — the label raises the verb's base risk.
    #[test]
    fn risk_classification_from_labels() {
        let save = control("button/save", ControlKind::Button, "Save", 0, 1);
        let del = control(
            "button/delete",
            ControlKind::Button,
            "Delete Database",
            0,
            2,
        );
        let next = control("button/next", ControlKind::Button, "Next", 0, 3);
        assert_eq!(
            classify_risk(&ActionVerb::Activate, Some(&save)),
            ActionRisk::Mutating
        );
        assert_eq!(
            classify_risk(&ActionVerb::Activate, Some(&del)),
            ActionRisk::Destructive,
            "delete-labelled activation must outrank plain mutation"
        );
        assert_eq!(
            classify_risk(&ActionVerb::Activate, Some(&next)),
            ActionRisk::Mutating
        );
        // Audit P0-4: focus plans as a mouse click — which ACTIVATES. It is
        // Mutating even on a destructive-labelled control (label evidence
        // does not apply: the click risk comes from the mechanism, not the
        // label), and never claims Safe again.
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&del)),
            ActionRisk::Mutating
        );
        // Focus is mutating on a neutral control too.
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&next)),
            ActionRisk::Mutating
        );
        // Risk ordering supports gating.
        assert!(ActionRisk::Safe < ActionRisk::Mutating);
        assert!(ActionRisk::Mutating < ActionRisk::Destructive);
        assert!(ActionRisk::Destructive < ActionRisk::ExternalSideEffect);
    }

    /// Verb/kind mismatches are typed errors, not best-effort actions.
    #[test]
    fn verb_kind_mismatch() {
        let btn = control("button/save", ControlKind::Button, "Save", 0, 1);
        let field = control("field/host", ControlKind::Field, "Host", 0, 2);
        assert!(matches!(
            plan_action(&ActionVerb::Toggle, &btn),
            Err(IntentError::VerbMismatch { .. })
        ));
        assert!(matches!(
            plan_action(&ActionVerb::Type { text: "x".into() }, &btn),
            Err(IntentError::VerbMismatch { .. })
        ));
        // Typing into a field plans a Type action.
        let planned = plan_action(
            &ActionVerb::Type {
                text: "localhost".into(),
            },
            &field,
        )
        .expect("type into field");
        assert!(matches!(planned, CanonicalAction::Type { .. }));
    }

    /// Key display names round-trip through the canonical parser: anything
    /// `display()` prints must parse back to the same event.
    #[test]
    fn key_display_roundtrips_through_parser() {
        let cases = vec![
            KeyEvent::new(KeyCode::Tab),
            KeyEvent::with_modifiers(KeyCode::Tab, KeyModifiers::SHIFT),
            KeyEvent::with_modifiers(KeyCode::Char('c'), KeyModifiers::CTRL),
            KeyEvent::new(KeyCode::Enter),
            KeyEvent::new(KeyCode::Char('q')),
            KeyEvent::new(KeyCode::Char(' ')),
            KeyEvent::new(KeyCode::Function(5)),
            // Shift+letter canonicalizes to the uppercase char (the parser's
            // rule), so feed the canonical form here.
            KeyEvent::with_modifiers(KeyCode::Char('S'), KeyModifiers::CTRL | KeyModifiers::SHIFT),
        ];
        for k in cases {
            let shown = k.display();
            let back = crate::mcp::helpers::parse_key_public(&shown)
                .unwrap_or_else(|e| panic!("'{shown}' must parse: {e}"));
            assert_eq!(back, k, "round-trip failed for '{shown}'");
        }
    }

    /// `ActionRisk::Unknown` is a distinct epistemic state (audit "action risk
    /// needs Unknown"): parsable, named, and — critically — it must NOT sort as
    /// if it were `Safe`, and it must demand confirmation like an unsafe action.
    #[test]
    fn unknown_risk_is_distinct_and_demands_confirmation() {
        // Serialization round-trip.
        assert_eq!(ActionRisk::parse("unknown"), Some(ActionRisk::Unknown));
        assert_eq!(ActionRisk::Unknown.name(), "unknown");
        assert!(
            ActionRisk::parse("safe").unwrap() != ActionRisk::Unknown,
            "unknown is not a downgrade of safe"
        );
        // The exploration fence `risk <= Safe` must exclude Unknown (a frame
        // with Unknown risk cannot silently pass a safe-only filter).
        assert!(
            ActionRisk::Unknown > ActionRisk::Safe,
            "unknown risk must not pass a safe-only filter"
        );
        // And it must be treated as needing confirmation, like destructive
        // side effects, not waved through.
        assert!(ActionRisk::Unknown.needs_confirmation());
        assert!(ActionRisk::Destructive.needs_confirmation());
        assert!(!ActionRisk::Safe.needs_confirmation());
    }
}
