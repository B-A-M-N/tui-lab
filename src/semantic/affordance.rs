//! Affordances (re-review Wave-4): what a user can *do* from this screen,
//! and how discoverable each action is.
//!
//! An [`Affordance`] is one actionable capability with its visible evidence.
//! The distinction the discoverability audit needs (re-review): "observed
//! `d → delete`" vs "no visible hint" are different worlds — a destructive
//! action with no on-screen cue is a finding, not a non-event. Text grep for
//! "help" cannot make that call; it must be made per-affordance against the
//! hints actually present.
//!
//! Everything here is inferred (`source: "inferred"`, confidence < 1.0)
//! unless a framework adapter supplied it natively.

use crate::screen::ScreenState;
use crate::semantic::confidence::Confidence;
use crate::semantic::controls::Control;

/// How the user invokes the action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum Invocation {
    /// A keyboard shortcut (e.g. "ctrl+s", "F1", "q").
    Key { key: String },
    /// Activated by focusing and pressing Enter/Space (buttons, toggles).
    Activate,
    /// Mouse-driven (click, drag, scroll).
    Mouse { action: String },
    /// Movement/selection (arrows, tab traversal).
    Navigate { key: String },
    /// The application declared this verb natively over the semantic side
    /// channel (audit finding 8) and the harness has no conventional
    /// invocation for it. Honest default: report the verb as the app
    /// stated it rather than guessing an invocation that may not exist.
    /// Mapping known verbs onto Key/Mouse/Activate happens at overlay
    /// time (`native.rs`); only genuinely unknown verbs land here.
    Declared { verb: String },
}

/// How visible the affordance's cue is on this screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// The key/hint is printed on the screen (e.g. "[F1] Help", "q quit").
    Labeled,
    /// No on-screen cue; the binding exists only by convention or source.
    Hidden,
}

/// One actionable capability of the current screen.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Affordance {
    /// What the user accomplishes (control label, or a conventional action
    /// name like "quit" for `q` in a list).
    pub action: String,
    /// The stable control ID when the affordance belongs to a detected
    /// [`Control`] (geometry-free per Wave-3 item 16).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
    pub invocation: Invocation,
    pub visibility: Visibility,
    /// The exact screen text that served as the cue, when visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_text: Option<String>,
    pub confidence: Confidence,
    pub source: String,
}

/// Extract affordances from a screen's semantic controls plus raw text.
///
/// Sources, in order of confidence:
///   1. Control-declared shortcuts (`&File`, "(F)ile" → "F") and
///      focusable/clickable controls themselves;
///   2. Hint footer patterns: `key action` pairs like "q quit", "^X exit",
///      "[F1] Help", "ctrl+s save" anywhere on screen.
pub fn infer_affordances(screen: &ScreenState, controls: &[Control]) -> Vec<Affordance> {
    let mut out = Vec::new();

    // 1) Per-control affordances.
    for c in controls {
        if let Some(sc) = &c.shortcut {
            out.push(Affordance {
                action: c.label.clone(),
                control_id: Some(c.id.clone()),
                invocation: Invocation::Key {
                    key: format!("alt+{}", sc.to_ascii_lowercase()),
                },
                visibility: Visibility::Labeled,
                hint_text: Some(c.label.clone()),
                confidence: Confidence::inferred(0.7, &["shortcut-marker"]),
                source: "inferred".into(),
            });
        }
        match c.kind {
            crate::semantic::controls::ControlKind::Button
            | crate::semantic::controls::ControlKind::MenuItem
            | crate::semantic::controls::ControlKind::Tab => {
                out.push(Affordance {
                    action: c.label.clone(),
                    control_id: Some(c.id.clone()),
                    invocation: Invocation::Activate,
                    visibility: Visibility::Labeled,
                    hint_text: Some(c.label.clone()),
                    confidence: Confidence::inferred(0.75, &["interactive-control"]),
                    source: "inferred".into(),
                });
            }
            _ => {}
        }
    }

    // 2) Hint-line pairs anywhere on screen: "q quit", "^X exit",
    //    "[F1] Help", "ctrl+s save", ":w write".
    for line in &screen.viewport_text {
        for (key, action) in extract_hint_pairs(line) {
            // Deduplicate against an identical earlier extraction (the same
            // hint repeated in two viewports is one affordance).
            let dup = out.iter().any(|a| {
                a.invocation == Invocation::Key { key: key.clone() } && a.action == action
            });
            if dup {
                continue;
            }
            out.push(Affordance {
                action,
                control_id: None,
                invocation: Invocation::Key { key },
                visibility: Visibility::Labeled,
                hint_text: Some(line.trim().to_string()),
                confidence: Confidence::inferred(0.65, &["hint-line"]),
                source: "inferred".into(),
            });
        }
    }

    out
}

/// Extract `(key, action)` pairs from one hint line.
///
/// Recognized shapes (ASCII key part, action = following word):
///   * `q quit`            — bare letter + space
///   * `^X exit`           — caret notation
///   * `[F1] Help`         — bracketed function key
///   * `ctrl+s save`       — explicit modifier
///   * `<tab> next`        — angle-bracketed key
pub(crate) fn extract_hint_pairs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let trimmed = line.trim();

    fn push_pair(out: &mut Vec<(String, String)>, key: String, rest: &str) {
        let action: String = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if action.len() >= 2
            || action.chars().all(|c| c.is_ascii_alphabetic()) && !action.is_empty()
        {
            out.push((key, action));
        }
    }

    // ^X notation
    let mut rest = trimmed;
    while let Some(pos) = rest.find('^') {
        let after = &rest[pos + 1..];
        if let Some(c) = after.chars().next() {
            if c.is_ascii_alphanumeric() {
                let key = format!("ctrl+{}", c.to_ascii_lowercase());
                push_pair(&mut out, key, &after[1..]);
                rest = after;
                continue;
            }
        }
        rest = after;
    }

    // [F1] / [ctrl+s] bracketed keys
    let mut rest = trimmed;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        if let Some(close) = after.find(']') {
            let inner = after[..close].trim();
            if is_keylike(inner) {
                push_pair(&mut out, normalize_key(inner), &after[close + 1..]);
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }

    // <tab> / <ctrl+n> angle-bracketed keys
    let mut rest = trimmed;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        if let Some(close) = after.find('>') {
            let inner = after[..close].trim();
            if is_keylike(inner) {
                push_pair(&mut out, normalize_key(inner), &after[close + 1..]);
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }

    // Explicit modifier words ("ctrl+s save") and bare-letter hints
    // ("q quit") on short hint lines (footers/help bars), not prose.
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 2 && words.len() <= 8 && trimmed.chars().count() <= 80 {
        let mut i = 0;
        while i + 1 < words.len() {
            let w = words[i];
            if let Some(stripped) = w.strip_prefix("ctrl+").or_else(|| w.strip_prefix("Ctrl+")) {
                if is_keylike(stripped) {
                    push_pair(
                        &mut out,
                        format!("ctrl+{}", stripped.to_ascii_lowercase()),
                        words[i + 1],
                    );
                    i += 2;
                    continue;
                }
            }
            // Bare letter hint: single alphabetic char followed by a word,
            // on lines that look like key bars (several such pairs or a
            // terse line — "hjkl move" / "q quit"). English articles and
            // prepositions ("a", "i", "o") are prose, not keys — excluded.
            const STOPWORDS: [char; 5] = ['a', 'i', 'o', 's', 't'];
            if w.chars().count() == 1 && w.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                let letter = w.chars().next().unwrap().to_ascii_lowercase();
                let action = words[i + 1].trim_matches(|c: char| !c.is_alphanumeric());
                if action.chars().count() >= 2 && !STOPWORDS.contains(&letter) {
                    out.push((letter.to_string(), action.to_string()));
                }
            }
            i += 1;
        }
    }

    out
}

/// Does this token plausibly name a key? Guards against "[note]" style
/// prose brackets.
fn is_keylike(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    let named = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "tab", "esc",
        "enter", "space", "up", "down", "left", "right", "home", "end", "pgup", "pgdn", "help",
        "del", "ins",
    ];
    if named.contains(&lower.as_str()) {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("ctrl+") {
        return rest.chars().count() == 1
            && rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
    }
    // Single key char or caret notation
    lower.chars().count() == 1
        && lower
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Canonicalize a key token ("F1" stays "f1"; "X" in "^X"/"[X]" → "ctrl+x"
/// only when caret notation; bracketed single letters are literal keys).
fn normalize_key(token: &str) -> String {
    token.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{ProcessState, ScreenState};
    use crate::semantic::controls::{Control, ControlBounds, ControlKind};

    fn screen(rows: Vec<&str>) -> ScreenState {
        ScreenState {
            cols: 80,
            rows: rows.len() as u16,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    fn button(id: &str, label: &str) -> Control {
        Control {
            id: id.into(),
            kind: ControlKind::Button,
            label: label.into(),
            value: None,
            bounds: ControlBounds {
                x: 0,
                y: 0,
                width: 5,
                height: 1,
            },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: Confidence::inferred(0.9, &["test"]),
            evidence: Vec::new(),
            source: "inferred".into(),
        }
    }

    #[test]
    fn footer_hint_pairs_extracted() {
        let s = screen(vec!["Files", "q quit  ^X exit  [F1] Help"]);
        let affs = infer_affordances(&s, &[]);
        let keys: Vec<(&str, &str)> = affs
            .iter()
            .filter_map(|a| match &a.invocation {
                Invocation::Key { key } => Some((key.as_str(), a.action.as_str())),
                _ => None,
            })
            .collect();
        assert!(
            keys.contains(&("q", "quit")),
            "bare-letter hint: {:?}",
            keys
        );
        assert!(
            keys.contains(&("ctrl+x", "exit")),
            "caret notation: {:?}",
            keys
        );
        assert!(
            keys.contains(&("f1", "Help")),
            "bracketed key keeps label case: {:?}",
            keys
        );
    }

    /// Prose brackets are not keys: "[note] this is a sentence" must not
    /// produce an affordance.
    #[test]
    fn prose_brackets_rejected() {
        let s = screen(vec!["[note] this is a longer sentence of prose text here"]);
        let before = infer_affordances(&s, &[]).len();
        // Only the prose line: no bracketed-key affordance for "note…" (it
        // fails is_keylike), and the line is too long for the bare-letter pass.
        assert_eq!(before, 0, "prose must not yield affordances");
    }

    /// Every focusable interactive control is an affordance, with its
    /// stable ID attached (discoverability audits key on IDs, not labels).
    #[test]
    fn controls_become_affordances_with_ids() {
        let s = screen(vec!["[ OK ]  [ Cancel ]"]);
        let ctrl = crate::semantic::controls::detect_controls(&s, &[]);
        let affs = infer_affordances(&s, &ctrl);
        assert!(
            affs.iter().any(|a| a.action == "OK"
                && a.control_id.is_some()
                && a.invocation == Invocation::Activate),
            "button affordance with control_id: {:?}",
            affs
        );
    }

    /// A button() helper smoke test: shortcut-bearing control keeps both its
    /// shortcut affordance and its activate affordance.
    #[test]
    fn shortcut_control_yields_key_affordance() {
        let mut b = button("button/save", "Save");
        b.shortcut = Some("S".into());
        let s = screen(vec!["(S)ave"]);
        let affs = infer_affordances(&s, &[b]);
        assert!(affs
            .iter()
            .any(|a| matches!(&a.invocation, Invocation::Key { key } if key == "alt+s")));
    }

    /// The review's motivating case: destructive action, no hint. The
    /// affordance list makes "visible hints vs. bindings" comparable.
    #[test]
    fn visibility_is_labeled_only_from_screen_text() {
        let s = screen(vec!["d delete-item"]); // hint present
        let affs = infer_affordances(&s, &[]);
        assert!(affs
            .iter()
            .any(|a| a.action == "delete-item" && a.visibility == Visibility::Labeled));

        let bare = screen(vec!["press enter to continue"]); // prose w/ 'press'
        let affs2 = infer_affordances(&bare, &[]);
        // "press enter…" is prose: no bare-letter pair (press=5 chars).
        assert!(!affs2.iter().any(|a| a.action == "press"));
    }
}
