//! The intent vocabulary: how an agent names targets, verbs, and risk,
//! and how resolution failures report themselves.
//!
//! Split from the former single-file `intent` (review §15 god-object
//! residue).

use crate::backend::{KeyCode, KeyEvent, MouseButton};
use crate::execution::CanonicalAction;
use crate::semantic::controls::{Control, ControlKind};
use crate::semantic::SemanticScreen;
use serde::{Deserialize, Serialize};

/// How an agent names what it wants to act on (Wave D item 31).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
            // Audit P0-4: `focus` plans as a MOUSE CLICK in this engine —
            // and a click on a button is an activation, not mere focus
            // traversal. Claiming Safe while sending a click let a
            // `max_risk=safe` explorer mutate the app while believing it
            // was only moving focus. Until a genuinely non-activating
            // focus primitive exists (a native focus event or verified
            // Tab traversal), the honest class is Mutating.
            ActionVerb::Focus => ActionRisk::Mutating,
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
    /// The risk cannot be proven from what is visible/known (an unknown TUI,
    /// an unmapped key, an experiment). Not a downgrade of the others — an
    /// honest "we don't know yet", deliberately distinct from `Safe`.
    Unknown,
}

impl ActionRisk {
    pub fn name(&self) -> &'static str {
        match self {
            ActionRisk::Safe => "safe",
            ActionRisk::Mutating => "mutating",
            ActionRisk::Destructive => "destructive",
            ActionRisk::ExternalSideEffect => "external_side_effect",
            ActionRisk::Unknown => "unknown",
        }
    }

    /// Parse the serialized form (MCP params, contracts).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "safe" => Some(ActionRisk::Safe),
            "mutating" => Some(ActionRisk::Mutating),
            "destructive" => Some(ActionRisk::Destructive),
            "external_side_effect" => Some(ActionRisk::ExternalSideEffect),
            "unknown" => Some(ActionRisk::Unknown),
            _ => None,
        }
    }

    /// The driving-risk fence: an action whose risk is unknown must not be
    /// assumed safe. Returns `true` when the risk implies a caller *should*
    /// seek confirmation before driving it (destructive, external, or unknown).
    pub fn needs_confirmation(&self) -> bool {
        matches!(
            self,
            ActionRisk::Destructive | ActionRisk::ExternalSideEffect | ActionRisk::Unknown
        )
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
/// raise the verb's base risk, never lower it (item 33).
pub fn classify_risk(verb: &ActionVerb, control: Option<&Control>) -> ActionRisk {
    let base = verb.base_risk();
    let Some(c) = control else { return base };
    // Focus/open never execute the control's action; label signals do not
    // apply to them. (Audit P0-4: focus is already Mutating at the base —
    // its click CAN activate — so there is no Safe shortcut to restore.)
    if matches!(verb, ActionVerb::Focus | ActionVerb::Open) {
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
