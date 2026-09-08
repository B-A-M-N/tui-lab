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
//! The resolved form is a plain `CanonicalAction`, so targeting composes
//! with the one canonical executor — settle waits, ledger, scenario capture —
//! without a second execution path.

mod keys;
mod plan;
mod vocab;

pub use plan::{plan_intent, plan_intent_with_graph, IntentPlan, PlannedStep};
pub use vocab::{
    classify_risk, nearest_candidates, plan_action, resolve_intent, resolve_target, ActionRisk,
    ActionTarget, ActionVerb, ControlSummary, IntentError, ResolvedIntent,
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
        // Mirror real analysis: the focused control's flag is set on the
        // Control, not only named in FocusInfo (the flat shape both agree).
        let mut controls = controls;
        if let Some(id) = focused {
            for c in controls.iter_mut() {
                c.focused = c.id == id;
            }
        }
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
    /// RESOLVED target, not wherever focus currently is. Finding 3B: the
    /// focus move is a NON-activating Tab traversal from the proven
    /// FocusGraph — never a click (which would fire the payload twice) —
    /// then the AssertFocus guard, then the single Enter payload.
    #[test]
    fn activation_plan_secures_focus_before_the_key() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"), // focus established on Cancel
        );
        // Proven route: Cancel --tab--> Save (recorded by a real traversal).
        let mut graph = crate::semantic::focus_graph::FocusGraph::new();
        graph.record_edge("button/cancel", "button/save", "tab", Some("Save"));
        let plan = plan_intent_with_graph(
            &s,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Activate,
            &graph,
        )
        .expect("plan");
        assert_eq!(plan.steps.len(), 3, "move → guard → act: {:?}", plan.steps);
        match &plan.steps[0] {
            PlannedStep::MoveFocus { target_id, key } => {
                assert_eq!(target_id, "button/save");
                // Finding 3B: the hop is Tab, NOT a click — clicking would
                // activate the target and the payload would fire twice.
                assert!(
                    matches!(key, CanonicalAction::Key { .. }),
                    "focus hop is a traversal key, never a click: {key:?}"
                );
            }
            other => panic!("step 0 must MoveFocus, got {other:?}"),
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

    /// Finding 3B, the headline invariant: a focus-secured activation plan
    /// contains EXACTLY ONE activating action — the payload. Every focus
    /// hop is Tab/Shift+Tab. This is the test that would have caught the
    /// click-then-Enter double-activation.
    #[test]
    fn activation_plan_never_contains_two_activating_actions() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        let mut graph = crate::semantic::focus_graph::FocusGraph::new();
        graph.record_edge("button/cancel", "button/save", "tab", Some("Save"));
        // Each verb on a legal kind for it, each with its own proven
        // route: Activate→button, Toggle→checkbox, Select→list, Open→menu
        // item. (Open on a button refuses at plan time — finding 3C.)
        let check = sem(
            vec![
                control("check/flag", ControlKind::Checkbox, "Flag", 0, 7),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        let list = sem(
            vec![
                control("list/lang", ControlKind::List, "Language", 0, 7),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        let menu = sem(
            vec![
                control("menu/file", ControlKind::MenuItem, "File", 0, 1),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        let mut check_graph = crate::semantic::focus_graph::FocusGraph::new();
        check_graph.record_edge("button/cancel", "check/flag", "tab", Some("Flag"));
        let mut list_graph = crate::semantic::focus_graph::FocusGraph::new();
        list_graph.record_edge("button/cancel", "list/lang", "tab", Some("Language"));
        let mut menu_graph = crate::semantic::focus_graph::FocusGraph::new();
        menu_graph.record_edge("button/cancel", "menu/file", "tab", Some("File"));
        for (screen, verb, target, graph) in [
            (
                &s,
                ActionVerb::Activate,
                ActionTarget::Id {
                    id: "button/save".into(),
                },
                &graph,
            ),
            (
                &check,
                ActionVerb::Toggle,
                ActionTarget::Id {
                    id: "check/flag".into(),
                },
                &check_graph,
            ),
            (
                &list,
                ActionVerb::Select,
                ActionTarget::Id {
                    id: "list/lang".into(),
                },
                &list_graph,
            ),
            (
                &menu,
                ActionVerb::Open,
                ActionTarget::Id {
                    id: "menu/file".into(),
                },
                &menu_graph,
            ),
        ] {
            let plan = plan_intent_with_graph(screen, &target, verb.clone(), graph)
                .unwrap_or_else(|e| panic!("{verb:?}: {e:?}"));
            let activating = plan
                .steps
                .iter()
                .filter(|st| {
                    matches!(
                        st,
                        PlannedStep::Act(CanonicalAction::Key { .. })
                            | PlannedStep::Act(CanonicalAction::MouseClick { .. })
                    )
                })
                .count();
            assert_eq!(
                activating, 1,
                "{verb:?}: exactly one activating action (the payload), got {activating} in {:?}",
                plan.steps
            );
            // And no click anywhere in a focus-secured plan.
            assert!(
                plan.steps.iter().all(|st| !matches!(
                    st,
                    PlannedStep::MoveFocus {
                        key: CanonicalAction::MouseClick { .. },
                        ..
                    }
                )),
                "{verb:?}: a focus hop must never be a click"
            );
        }
    }

    /// Finding 3A: `focus` plans a NON-activating traversal with NO payload
    /// (MoveFocus… → AssertFocus), or refuses when no proven route exists.
    /// It NEVER plans a click.
    #[test]
    fn focus_verb_plans_tab_traversal_not_click() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 5),
                control("button/cancel", ControlKind::Button, "Cancel", 10, 5),
            ],
            Some("button/cancel"),
        );
        let mut graph = crate::semantic::focus_graph::FocusGraph::new();
        graph.record_edge("button/cancel", "button/save", "tab", Some("Save"));
        let plan = plan_intent_with_graph(
            &s,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Focus,
            &graph,
        )
        .expect("focus plans from a proven route");
        // Traversal + guard, and NO Act payload.
        assert!(
            matches!(plan.steps.last(), Some(PlannedStep::AssertFocus { .. })),
            "focus ends in the guard: {:?}",
            plan.steps
        );
        assert!(
            plan.steps
                .iter()
                .all(|st| !matches!(st, PlannedStep::Act(_))),
            "focus has no payload: {:?}",
            plan.steps
        );
        // Risk is honestly Safe now: the mechanism is pure traversal.
        assert_eq!(plan.risk, ActionRisk::Safe);

        // No proven route → Unsupported (with the remedy), never a click.
        let empty = crate::semantic::focus_graph::FocusGraph::new();
        let err = plan_intent_with_graph(
            &s,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Focus,
            &empty,
        )
        .expect_err("no proven route must refuse");
        assert!(matches!(err, IntentError::Unsupported { .. }), "{err:?}");
        // The refusal names the remedy (prove the traversal), not a click.
        assert!(
            err.message().contains("NOT substituted"),
            "the refusal must state that clicking was not substituted: {}",
            err.message()
        );

        // Already focused: guard only, nothing sent.
        let s_focused = sem(
            vec![control("button/save", ControlKind::Button, "Save", 0, 5)],
            Some("button/save"),
        );
        let plan = plan_intent_with_graph(
            &s_focused,
            &ActionTarget::Id {
                id: "button/save".into(),
            },
            ActionVerb::Focus,
            &empty,
        )
        .expect("already-focused is guard-only");
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.steps);
    }

    /// Finding 3C: `open` is only legal on known-openable roles; a button
    /// is VerbMismatch (the agent should say `activate`), and open is no
    /// longer classified Safe.
    #[test]
    fn open_is_restricted_to_openable_roles_and_never_safe() {
        let btn = control("button/save", ControlKind::Button, "Save", 0, 1);
        let item = control("menu/file", ControlKind::MenuItem, "File", 0, 1);
        assert!(
            matches!(
                plan_action(&ActionVerb::Open, &btn),
                Err(IntentError::VerbMismatch { .. })
            ),
            "open on a button must refuse — an Enter there is an activation"
        );
        assert!(plan_action(&ActionVerb::Open, &item).is_ok());
        // Risk honesty: opening a menu changes app state.
        assert_eq!(ActionVerb::Open.base_risk(), ActionRisk::Mutating);
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

    /// Finding 28: the role slug is the SERDE wire name — `menu_item`
    /// (snake_case), not Debug's `menuitem`. A `role: "menu_item"` target
    /// must resolve; the same slug must appear in ControlSummary.kind so
    /// ambiguous/not-found error payloads echo what the caller sent.
    #[test]
    fn role_slug_is_the_serde_wire_name_not_debug_spelling() {
        let s = sem(
            vec![
                control("menu/open", ControlKind::MenuItem, "Open File", 0, 0),
                control("menu/quit", ControlKind::MenuItem, "Quit", 0, 1),
            ],
            None,
        );
        let c = resolve_target(
            &s,
            &ActionTarget::Role {
                role: "menu_item".into(),
                text: Some("Open File".into()),
            },
        )
        .expect("menu_item must match — snake_case, the wire name");
        assert_eq!(c.id, "menu/open");
        // Debug's "menuitem" is NOT accepted: it was the drift surface.
        assert!(matches!(
            resolve_target(
                &s,
                &ActionTarget::Role {
                    role: "menuitem".into(),
                    text: None
                }
            ),
            Err(IntentError::NotFound { .. })
        ));
        // Error payloads carry the wire slug too.
        let err = resolve_target(
            &s,
            &ActionTarget::Role {
                role: "menu_item".into(),
                text: None,
            },
        );
        match err {
            Err(IntentError::Ambiguous { matches, .. }) => {
                assert!(
                    matches.iter().all(|m| m.kind == "menu_item"),
                    "summaries echo the wire slug: {matches:?}"
                );
            }
            other => panic!("two menu items → ambiguous, got {other:?}"),
        }
    }

    /// Finding 28: nearest candidates are FOCUSED FIRST — the doc always
    /// promised it; the sort now delivers it even when the focused control
    /// is textually farther from the target.
    #[test]
    fn nearest_candidates_sort_focused_first() {
        let s = sem(
            vec![
                control("button/save", ControlKind::Button, "Save", 0, 0),
                control("button/close", ControlKind::Button, "Close", 0, 1),
            ],
            Some("button/close"),
        );
        // "sve" matches nothing exactly or by substring → NotFound with
        // candidates; the focused "Close" must still lead.
        let err = resolve_target(&s, &ActionTarget::Text { text: "sve".into() })
            .expect_err("no 'sve' control → NotFound");
        let IntentError::NotFound { candidates, .. } = err else {
            panic!("expected NotFound, got {err:?}")
        };
        assert!(
            candidates.first().map(|c| c.focused).unwrap_or(false),
            "focused control sorts first: {candidates:?}"
        );
        assert_eq!(candidates.len(), 2, "both interactive controls offered");
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
        // Finding 3A: focus is now a guarded Tab traversal that never
        // touches the control's action — honestly Safe (the doc's risk
        // ladder names focus movement as the Safe case).
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&del)),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&next)),
            ActionRisk::Safe
        );
        // Finding 3C: open executes the control's action on an openable
        // role — mutating at base, raised by label evidence like activate.
        assert_eq!(
            classify_risk(&ActionVerb::Open, Some(&del)),
            ActionRisk::Destructive
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
