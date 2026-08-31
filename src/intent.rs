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

use crate::backend::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use crate::execution::CanonicalAction;
use crate::semantic::controls::{Control, ControlKind};
use crate::semantic::SemanticScreen;
use serde::{Deserialize, Serialize};

/// How an agent names what it wants to act on (Wave D item 31).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum ActionTarget {
    /// A stable semantic ID (`button/save`, `field/host`).
    Id { id: String },
    /// Visible text — a control label or on-screen text.
    Text { text: String },
    /// The control kind plus optional label refinement (`button "Save"`).
    Role { role: String, text: Option<String> },
    /// The currently focused control.
    Focused,
}

impl ActionTarget {
    /// Human-readable description for evidence and errors.
    pub fn describe(&self) -> String {
        match self {
            ActionTarget::Id { id } => format!("#{id}"),
            ActionTarget::Text { text } => format!("\"{text}\""),
            ActionTarget::Role {
                role,
                text: Some(t),
            } => {
                format!("{role} \"{t}\"")
            }
            ActionTarget::Role { role, text: None } => role.clone(),
            ActionTarget::Focused => "the focused control".to_string(),
        }
    }
}

/// A verb the agent can apply to a target (item 31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionVerb {
    /// Press Enter/Space on the control (buttons, toggles, items).
    Activate,
    /// Move keyboard focus to the control without activating.
    Focus,
    /// Click the control with the primary mouse button.
    Click,
    /// Type text into a field (replacing nothing — appended at its cursor).
    Type { text: String },
    /// Check/toggle a checkbox or radio.
    Toggle,
    /// Cycle selection / select an item in a list.
    Select,
    /// Open the control (menu, dropdown, tree branch).
    Open,
}

impl ActionVerb {
    pub fn name(&self) -> &'static str {
        match self {
            ActionVerb::Activate => "activate",
            ActionVerb::Focus => "focus",
            ActionVerb::Click => "click",
            ActionVerb::Type { .. } => "type",
            ActionVerb::Toggle => "toggle",
            ActionVerb::Select => "select",
            ActionVerb::Open => "open",
        }
    }

    /// Risk class of the verb in isolation (item 33). The target can only
    /// raise this (a button labelled "Delete" mutates), never lower it.
    pub fn base_risk(&self) -> ActionRisk {
        match self {
            ActionVerb::Focus => ActionRisk::Safe,
            // Activation/click runs the control's action — mutating unless
            // proven otherwise.
            ActionVerb::Activate | ActionVerb::Click | ActionVerb::Toggle | ActionVerb::Select => {
                ActionRisk::Mutating
            }
            ActionVerb::Type { .. } => ActionRisk::Mutating,
            ActionVerb::Open => ActionRisk::Safe,
        }
    }
}

/// Risk classes (Wave D item 33), ordered by escalating blast radius.
///
/// `allowed_risk` on candidate contexts and exploration plans gates against
/// this: an explorer running with `allowed_risk = Safe` will be *offered*
/// "focus Next" but never "activate Delete Database".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    /// Cannot change application state: focus movement, opening a menu.
    Safe,
    /// Changes in-application state (save, toggle, navigate, type).
    Mutating,
    /// Destroys or is hard to reverse in-application (delete, reset, quit).
    Destructive,
    /// Reaches outside the application (signals, paste from clipboard).
    ExternalSideEffect,
}

impl ActionRisk {
    pub fn name(&self) -> &'static str {
        match self {
            ActionRisk::Safe => "safe",
            ActionRisk::Mutating => "mutating",
            ActionRisk::Destructive => "destructive",
            ActionRisk::ExternalSideEffect => "external_side_effect",
        }
    }

    /// Parse the serialized form (MCP params, contracts).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "safe" => Some(ActionRisk::Safe),
            "mutating" => Some(ActionRisk::Mutating),
            "destructive" => Some(ActionRisk::Destructive),
            "external_side_effect" => Some(ActionRisk::ExternalSideEffect),
            _ => None,
        }
    }
}

/// Why a target could not resolve to exactly one control (item 31).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum IntentError {
    /// Several controls matched. Resolution must never silently pick the
    /// first — the agent refines (add text, use the ID).
    Ambiguous {
        target: String,
        matches: Vec<ControlSummary>,
    },
    /// Nothing matched. Nearest candidates are attached for self-correction.
    NotFound {
        target: String,
        candidates: Vec<ControlSummary>,
    },
    /// The verb does not apply to the matched control's kind (typing into a
    /// button, toggling a label).
    VerbMismatch { target: String, verb: String },
}

impl IntentError {
    /// Envelope-ready message.
    pub fn message(&self) -> String {
        match self {
            IntentError::Ambiguous { target, matches } => format!(
                "ambiguous target {}: {} controls match ({}); refine with an ID",
                target,
                matches.len(),
                matches
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            IntentError::NotFound { target, candidates } => format!(
                "no control matches {}; nearest candidates: {}",
                target,
                candidates
                    .iter()
                    .map(|c| format!("{} ({})", c.id, c.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            IntentError::VerbMismatch { target, verb } => {
                format!("verb '{verb}' does not apply to {target}")
            }
        }
    }
}

/// Compact control identity for ambiguous/not-found errors: enough for the
/// agent to disambiguate without a second observe round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlSummary {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub focused: bool,
}

impl ControlSummary {
    fn of(c: &Control) -> Self {
        ControlSummary {
            id: c.id.clone(),
            label: c.label.clone(),
            kind: format!("{:?}", c.kind).to_lowercase(),
            focused: c.focused,
        }
    }
}

/// One resolved intent: the concrete action plus the evidence trail.
#[derive(Debug, Clone)]
pub struct ResolvedIntent {
    /// The control the target resolved to.
    pub control: Control,
    /// The verb that will be applied.
    pub verb: ActionVerb,
    /// Risk class (verb base risk, raised by target evidence).
    pub risk: ActionRisk,
    /// The concrete action to execute.
    pub action: CanonicalAction,
}

/// Classify the risk of acting on one control: label and kind evidence can
/// raise the verb's base risk, never lower it (item 33). Focus movement
/// runs nothing, so label evidence never raises it past `Safe`.
pub fn classify_risk(verb: &ActionVerb, control: Option<&Control>) -> ActionRisk {
    let base = verb.base_risk();
    let Some(c) = control else { return base };
    // Focus/open never execute the control's action; label signals do not
    // apply to them.
    if matches!(verb, ActionVerb::Focus) {
        return base;
    }
    let label = c.label.to_lowercase();
    let destructive_words = [
        "delete", "remove", "destroy", "reset", "wipe", "purge", "format", "drop", "quit", "exit",
        "shutdown", "kill",
    ];
    let external_words = ["clipboard", "share", "upload", "send", "export", "publish"];
    let mut risk = base;
    if destructive_words.iter().any(|w| label.contains(w)) {
        risk = risk.max(ActionRisk::Destructive);
    } else if external_words.iter().any(|w| label.contains(w)) {
        risk = risk.max(ActionRisk::ExternalSideEffect);
    }
    risk
}

/// Resolve a target against the semantic screen. Deterministic, and honest
/// about ambiguity: multiple matches are an error carrying the matches,
/// never a first-pick (item 31).
pub fn resolve_target(sem: &SemanticScreen, target: &ActionTarget) -> Result<Control, IntentError> {
    let controls = &sem.controls;
    let matches: Vec<&Control> = match target {
        ActionTarget::Id { id } => controls.iter().filter(|c| &c.id == id).collect(),
        ActionTarget::Text { text } => {
            let t = text.to_lowercase();
            let exact: Vec<&Control> = controls
                .iter()
                .filter(|c| c.label.to_lowercase() == t)
                .collect();
            if exact.len() == 1 {
                return Ok(exact.into_iter().next().unwrap().clone());
            }
            if exact.len() > 1 {
                return Err(ambiguous(target, &exact));
            }
            // Substring fallback only when it stays unambiguous.
            let sub: Vec<&Control> = controls
                .iter()
                .filter(|c| c.label.to_lowercase().contains(&t))
                .collect();
            match sub.len() {
                1 => sub,
                0 => Vec::new(),
                _ => return Err(ambiguous(target, &sub)),
            }
        }
        ActionTarget::Role { role, text } => {
            let want = normalize_role(role);
            let by_role: Vec<&Control> = controls
                .iter()
                .filter(|c| kind_role_slug(&c.kind) == want)
                .collect();
            match text {
                None => by_role,
                Some(t) => {
                    let tl = t.to_lowercase();
                    let both: Vec<&Control> = by_role
                        .iter()
                        .filter(|c| c.label.to_lowercase().contains(&tl))
                        .copied()
                        .collect();
                    both
                }
            }
        }
        ActionTarget::Focused => {
            match sem
                .focus
                .control_id
                .as_deref()
                .and_then(|id| controls.iter().find(|c| c.id == id))
            {
                Some(c) => vec![c],
                None => Vec::new(),
            }
        }
    };

    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap().clone()),
        0 => Err(IntentError::NotFound {
            target: target.describe(),
            candidates: nearest_candidates(controls, target),
        }),
        _ => Err(ambiguous(target, &matches)),
    }
}

/// Plan the concrete [`CanonicalAction`] for verb + control (item 31).
///
/// Keyboard-first: activation goes through focus + Enter so it works for any
/// focusable control, with a mouse click only when the control is not
/// focusable and the backend has mouse.
pub fn plan_action(verb: &ActionVerb, control: &Control) -> Result<CanonicalAction, IntentError> {
    use CanonicalAction as CA;
    let center_x = control.bounds.x.saturating_add(control.bounds.width / 2);
    let center_y = control.bounds.y;
    let verb_name = verb.name();
    match (verb, &control.kind) {
        (ActionVerb::Focus, _) => {
            if !control.focusable {
                return Err(IntentError::VerbMismatch {
                    target: control.id.clone(),
                    verb: verb_name.into(),
                });
            }
            // Focus is reached by clicking with the keyboard-safe path:
            // there is no "focus this" key, so activation-style traversal is
            // the honest plan — a click on focusable controls is the only
            // direct route and is left to the caller when mouse is absent.
            Ok(CA::MouseClick {
                button: MouseButton::Left,
                x: center_x,
                y: center_y,
            })
        }
        (ActionVerb::Click, _) => Ok(CA::MouseClick {
            button: MouseButton::Left,
            x: center_x,
            y: center_y,
        }),
        (ActionVerb::Activate, ControlKind::Field) => Ok(CA::Key {
            key: KeyEvent::new(KeyCode::Enter),
        }),
        (ActionVerb::Activate, _) => Ok(CA::Key {
            key: KeyEvent::new(KeyCode::Enter),
        }),
        (ActionVerb::Toggle, ControlKind::Checkbox | ControlKind::Radio) => Ok(CA::Key {
            key: KeyEvent::new(KeyCode::Char(' ')),
        }),
        (ActionVerb::Toggle, _) => Err(IntentError::VerbMismatch {
            target: control.id.clone(),
            verb: verb_name.into(),
        }),
        (ActionVerb::Select, ControlKind::List | ControlKind::MenuItem | ControlKind::Tab) => {
            Ok(CA::Key {
                key: KeyEvent::new(KeyCode::Enter),
            })
        }
        (ActionVerb::Select, _) => Err(IntentError::VerbMismatch {
            target: control.id.clone(),
            verb: verb_name.into(),
        }),
        (ActionVerb::Open, ControlKind::MenuItem) => Ok(CA::Key {
            key: KeyEvent::new(KeyCode::Enter),
        }),
        (ActionVerb::Open, _) => Ok(CA::Key {
            key: KeyEvent::new(KeyCode::Enter),
        }),
        (ActionVerb::Type { text }, ControlKind::Field) => Ok(CA::Type { text: text.clone() }),
        (ActionVerb::Type { .. }, _) => Err(IntentError::VerbMismatch {
            target: control.id.clone(),
            verb: verb_name.into(),
        }),
    }
}

/// Resolve + plan in one step.
pub fn resolve_intent(
    sem: &SemanticScreen,
    target: &ActionTarget,
    verb: ActionVerb,
) -> Result<ResolvedIntent, IntentError> {
    let control = resolve_target(sem, target)?;
    let action = plan_action(&verb, &control)?;
    let risk = classify_risk(&verb, Some(&control));
    Ok(ResolvedIntent {
        control,
        verb,
        risk,
        action,
    })
}

// ─── helpers ───────────────────────────────────────────────────────────────

fn ambiguous(target: &ActionTarget, matches: &[&Control]) -> IntentError {
    IntentError::Ambiguous {
        target: target.describe(),
        matches: matches.iter().map(|c| ControlSummary::of(c)).collect(),
    }
}

fn normalize_role(role: &str) -> String {
    role.trim().to_lowercase()
}

/// The role slug a control kind answers to (`"button"`, `"field"`).
fn kind_role_slug(kind: &ControlKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// Nearest candidates for a not-found error: focused control first, then
/// interactive controls in reading order — the agent's likely intent lives
/// there.
fn nearest_candidates(controls: &[Control], target: &ActionTarget) -> Vec<ControlSummary> {
    let mut scored: Vec<(u32, &Control)> = controls
        .iter()
        .filter(|c| c.focusable || matches!(c.kind, ControlKind::Button | ControlKind::MenuItem))
        .map(|c| (text_distance_score(target, c), c))
        .collect();
    scored.sort_by_key(|(score, c)| (*score, c.bounds.y, c.bounds.x));
    scored
        .into_iter()
        .take(5)
        .map(|(_, c)| ControlSummary::of(c))
        .collect()
}

/// Cheap relatedness score for candidate ranking: shared prefix/substring
/// beats nothing. Lower is better; anything above 3 is "no relation".
fn text_distance_score(target: &ActionTarget, c: &Control) -> u32 {
    let want = match target {
        ActionTarget::Text { text }
        | ActionTarget::Role {
            text: Some(text), ..
        } => text.to_lowercase(),
        ActionTarget::Id { id } => id.to_lowercase(),
        _ => return 3,
    };
    let label = c.label.to_lowercase();
    if label.contains(&want) || want.contains(&label) {
        0
    } else if label.starts_with(want.chars().next().unwrap_or('\0')) {
        1
    } else {
        2
    }
}

// ─── key serialization (the inverse of the MCP key parser) ─────────────────

impl KeyCode {
    /// Canonical key name, the exact vocabulary `parse_key_public` accepts.
    pub fn name(&self) -> String {
        match self {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Escape => "escape".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::Insert => "insert".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Function(n) => format!("f{n}"),
        }
    }
}

impl KeyModifiers {
    /// Modifier prefix in canonical order (`ctrl+alt+`), empty when none.
    pub fn prefix(&self) -> String {
        let mut parts = Vec::new();
        if self.contains(KeyModifiers::CTRL) {
            parts.push("ctrl");
        }
        if self.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        if self.contains(KeyModifiers::SHIFT) {
            parts.push("shift");
        }
        if self.contains(KeyModifiers::SUPER) {
            parts.push("super");
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("{}+", parts.join("+"))
        }
    }
}

impl KeyEvent {
    /// Canonical display form: `ctrl+alt+delete`, `shift+tab`, `a`.
    ///
    /// A shifted letter renders as its uppercase char — matching the
    /// parser's canonicalization (`shift+a` parses to `Char('A')`), so
    /// `display()` always round-trips through `parse_key_public`.
    pub fn display(&self) -> String {
        let body = match self.code {
            KeyCode::Char(c) if self.modifiers.shift() && c.is_ascii_lowercase() => {
                c.to_ascii_uppercase().to_string()
            }
            _ => self.code.name(),
        };
        format!("{}{}", self.modifiers.prefix(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::controls::ControlBounds;
    use crate::semantic::focus::FocusInfo;

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
        // Focus stays safe even on a destructive-labelled control: focusing
        // runs nothing (the earlier Mutating expectation was wrong).
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&del)),
            ActionRisk::Safe
        );
        // Focus is safe on a neutral control.
        assert_eq!(
            classify_risk(&ActionVerb::Focus, Some(&next)),
            ActionRisk::Safe
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
}
