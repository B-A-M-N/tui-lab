//! Interaction-level risk classification (re-review items 27/28).
//!
//! [`crate::intent::classify_risk`] classifies *intents* — verb + control.
//! This module classifies *raw actions*: a `CanonicalAction` arrives over
//! the wire or from an exploration pool with no control attached, and the
//! question "may an autonomous driver execute this without asking?" must
//! still have a deterministic answer.
//!
//! Two fences the re-review calls out by name:
//!
//! * **The commit fence** (item 27): labels that plausibly commit work or
//!   reach an outside system — Save, Submit, Apply, Deploy, Connect, Send,
//!   Authorize — are never auto-clicked by brownfield-safe drivers. Clicks
//!   on them remain available to the caller (`tui_act`), but the audit and
//!   exploration engines classify them [`ActionRisk::Mutating`] at minimum
//!   and skip them under a safe allowance.
//! * **The Escape fence** (item 28): Escape is not safe by default. It
//!   cancels dialogs, discards forms, closes the app in some TUIs. Unless
//!   the app's own evidence (contract keybindings, on-screen affordance)
//!   proves it harmless, it classifies [`ActionRisk::Unknown`] — which the
//!   driving-risk fence treats as not-assumed-safe.

use crate::execution::CanonicalAction;
use crate::intent::ActionRisk;

/// Labels that plausibly commit work or reach outside the process (item 27).
/// A click on any of these can change persistent or external state with no
/// undo; autonomous drivers must not select them as probe targets.
pub const COMMIT_LABELS: &[&str] = &[
    "save",
    "submit",
    "apply",
    "deploy",
    "connect",
    "send",
    "authorize",
];

/// Labels that destroy in-process state (carried over from the mouse
/// audit's original filter, now shared).
pub const DESTRUCTIVE_LABELS: &[&str] = &[
    "delete", "remove", "quit", "exit", "kill", "reset", "format", "erase", "destroy", "wipe",
    "purge", "shutdown",
];

/// Labels that reach outside the process (network, clipboard, publishing).
pub const EXTERNAL_LABELS: &[&str] = &[
    "upload", "publish", "export", "share", "download", "fetch", "sync",
];

/// True when `label` (case-insensitive) contains a commit-fence word.
pub fn is_commit_label(label: &str) -> bool {
    let l = label.to_lowercase();
    COMMIT_LABELS.iter().any(|w| l.contains(w))
}

/// True when `label` matches any non-safe evidence class.
pub fn is_non_safe_label(label: &str) -> bool {
    let l = label.to_lowercase();
    COMMIT_LABELS
        .iter()
        .chain(DESTRUCTIVE_LABELS.iter())
        .chain(EXTERNAL_LABELS.iter())
        .any(|w| l.contains(w))
}

/// Classify a raw canonical action for an autonomous driver (items 27/28).
///
/// The result is the risk the action carries *without* target evidence —
/// target evidence (a resolved control) can only raise it, via
/// [`classify_with_label`]. Mouse actions split by class:
///
/// ```text
/// MouseMove / MouseScroll        Safe       hovering and wheeling change
///                                           no application state by
///                                           convention; an app that
///                                           violates that is a finding,
///                                           not a classification bug
/// MouseClick / Press / Release   Mutating   a click RUNS something unless
///                                           proven otherwise
/// Raw                            Unknown    arbitrary bytes — no claim
/// Signal                         ExternalSideEffect
/// ```
///
/// Keys: navigation and editing keys are `Safe`; Enter/Space (activation)
/// and printable text are `Mutating`; **Escape is `Unknown`** (item 28) —
/// it cancels dialogs and discards edits in most TUIs, and "probably
/// harmless" is not evidence.
pub fn classify_action(action: &CanonicalAction) -> ActionRisk {
    match action {
        CanonicalAction::MouseMove { .. } | CanonicalAction::MouseScroll { .. } => ActionRisk::Safe,
        CanonicalAction::MouseClick { .. }
        | CanonicalAction::MousePress { .. }
        | CanonicalAction::MouseRelease { .. }
        | CanonicalAction::MouseDrag { .. } => ActionRisk::Mutating,
        // Resize changes only the viewport, and the audit restores it;
        // reversible in the MutationRisk sense but classified conservatively
        // here because an app may persist geometry on resize.
        CanonicalAction::Resize { .. } => ActionRisk::Mutating,
        CanonicalAction::Signal { .. } => ActionRisk::ExternalSideEffect,
        CanonicalAction::Raw { .. } => ActionRisk::Unknown,
        CanonicalAction::Paste { .. } => ActionRisk::ExternalSideEffect,
        CanonicalAction::Type { .. } => ActionRisk::Mutating,
        CanonicalAction::Keys { keys } => keys
            .iter()
            .map(|k| classify_key(&k.code))
            .max()
            .unwrap_or(ActionRisk::Safe),
        CanonicalAction::Key { key } => classify_key(&key.code),
    }
}

/// Key-level classification. `context` is the app's own evidence about the
/// key (contract keybindings / affordances); `Some` proves the key does
/// something declared, which is enough to pin Escape at `Mutating` instead
/// of `Unknown` — a declared binding is documented behavior, not a guess.
pub fn classify_key_with(key: &crate::backend::KeyCode, context: bool) -> ActionRisk {
    let base = classify_key(key);
    match (base, context) {
        (ActionRisk::Unknown, true) => ActionRisk::Mutating,
        (risk, _) => risk,
    }
}

fn classify_key(code: &crate::backend::KeyCode) -> ActionRisk {
    use crate::backend::KeyCode as K;
    match code {
        // Activation runs the focused control's action.
        K::Enter | K::Char(' ') => ActionRisk::Mutating,
        // Item 28: Escape is not safe by default.
        K::Escape => ActionRisk::Unknown,
        // Tab/arrows/Home/End/PageUp/PageDown move focus or scroll.
        K::Tab
        | K::Up
        | K::Down
        | K::Left
        | K::Right
        | K::Home
        | K::End
        | K::PageUp
        | K::PageDown
        | K::Insert
        | K::Delete
        | K::Backspace => ActionRisk::Safe,
        // Everything else (printable chars, F-keys) may be bound to
        // anything — text mutates a field, an F-key is unproven intent.
        K::Char(_) | K::Function(_) => ActionRisk::Mutating,
    }
}

/// Raise a raw-action class with target evidence: a resolved control's
/// label. Commit-fence labels force at least `Mutating` even when the raw
/// class was lower (a click is already Mutating; this documents the label
/// evidence); destructive labels force `Destructive`; external labels
/// force `ExternalSideEffect`. Never lowers.
pub fn classify_with_label(base: ActionRisk, label: &str) -> ActionRisk {
    let l = label.to_lowercase();
    if DESTRUCTIVE_LABELS.iter().any(|w| l.contains(w)) {
        return base.max(ActionRisk::Destructive);
    }
    if EXTERNAL_LABELS.iter().any(|w| l.contains(w)) {
        return base.max(ActionRisk::ExternalSideEffect);
    }
    if is_commit_label(label) {
        return base.max(ActionRisk::Mutating);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent};

    fn k(code: KeyCode) -> CanonicalAction {
        CanonicalAction::Key {
            key: KeyEvent::new(code),
        }
    }

    #[test]
    fn mouse_classes_split_by_subclass() {
        assert_eq!(
            classify_action(&CanonicalAction::MouseMove { x: 1, y: 1 }),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_action(&CanonicalAction::MouseScroll {
                direction: crate::backend::ScrollDirection::Down,
                x: 1,
                y: 1
            }),
            ActionRisk::Safe
        );
        assert_eq!(
            classify_action(&CanonicalAction::MouseClick {
                button: crate::backend::MouseButton::Left,
                x: 1,
                y: 1
            }),
            ActionRisk::Mutating
        );
    }

    /// Item 27: the commit fence. These labels are never treated as safe
    /// click targets regardless of control kind.
    #[test]
    fn commit_labels_are_fenced() {
        for w in [
            "Save",
            "Submit",
            "Apply",
            "Deploy",
            "Connect",
            "Send",
            "Authorize",
        ] {
            assert!(is_commit_label(w), "{w} must be fenced");
            assert_eq!(
                classify_with_label(ActionRisk::Safe, &format!(" {} ", w)),
                ActionRisk::Mutating
            );
        }
    }

    #[test]
    fn destructive_beats_external_beats_commit() {
        assert_eq!(
            classify_with_label(ActionRisk::Safe, "delete and send"),
            ActionRisk::Destructive
        );
        assert_eq!(
            classify_with_label(ActionRisk::Safe, "publish draft"),
            ActionRisk::ExternalSideEffect
        );
    }

    /// Item 28: Escape is Unknown by default, Mutating only when the app's
    /// own evidence (a declared binding) proves what it does.
    #[test]
    fn escape_is_not_safe_by_default() {
        assert_eq!(classify_action(&k(KeyCode::Escape)), ActionRisk::Unknown);
        assert!(
            ActionRisk::Unknown > ActionRisk::Mutating,
            "Unknown must not pass a mutating gate"
        );
        assert_eq!(
            classify_key_with(&KeyCode::Escape, true),
            ActionRisk::Mutating,
            "declared context proves Escape's behavior"
        );
        assert_eq!(
            classify_key_with(&KeyCode::Escape, false),
            ActionRisk::Unknown
        );
    }

    #[test]
    fn navigation_keys_stay_safe() {
        for c in [KeyCode::Tab, KeyCode::Down, KeyCode::Home, KeyCode::PageUp] {
            assert_eq!(classify_action(&k(c)), ActionRisk::Safe, "{c:?}");
        }
        assert_eq!(classify_action(&k(KeyCode::Enter)), ActionRisk::Mutating);
        assert_eq!(
            classify_action(&k(KeyCode::Function(5))),
            ActionRisk::Mutating
        );
        assert_eq!(
            classify_action(&k(KeyCode::Char('x'))),
            ActionRisk::Mutating
        );
    }

    #[test]
    fn signal_and_raw_are_not_safe() {
        assert_eq!(
            classify_action(&CanonicalAction::Signal { signal: 15 }),
            ActionRisk::ExternalSideEffect
        );
        assert_eq!(
            classify_action(&CanonicalAction::Raw { bytes: vec![0x1b] }),
            ActionRisk::Unknown
        );
    }
}
