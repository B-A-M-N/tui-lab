//! Shared helpers for the MCP tools layer: envelope rendering, input/wait
//! builders, assertion runner, cast writer, and small global stores.

use crate::error::{Envelope, ErrorCategory};
use crate::screen::ScreenState;
use rmcp::model::CallToolResult;
use std::time::Duration;

use crate::backend::{KeyEvent, KeyModifiers};

/// Render a success envelope as a structured tool result (Wave G item 71):
/// `structured_content` carries the envelope object; the same JSON rides in
/// the text block for clients that only read text. Assertion-style
/// `passed: false` stays a success at the transport level (see
/// [`err_continued`]); genuine failures set `is_error` only when the
/// category is a caller/transport fault, so agents can branch on the
/// payload either way.
pub fn ok(data: serde_json::Value) -> CallToolResult {
    let env = Envelope::ok(data);
    from_envelope_json(&env.to_json(), false)
}

/// Render a hard-failure envelope (category + message). `is_error: true` —
/// this is a real fault (invalid request, backend error, …), not a UI
/// verdict.
pub fn err(cat: ErrorCategory, msg: impl Into<String>) -> CallToolResult {
    let env: Envelope<()> = Envelope::<()>::fail(cat, msg);
    from_envelope_json(&env.to_json(), true)
}

/// Wrap an already-rendered envelope JSON string into a CallToolResult.
fn from_envelope_json(json: &str, is_error: bool) -> CallToolResult {
    let value: serde_json::Value = serde_json::from_str(json)
        .unwrap_or(serde_json::Value::Null);
    let mut r = CallToolResult::structured(value);
    r.content = vec![rmcp::model::ContentBlock::text(json)];
    r.is_error = Some(is_error);
    r
}

/// Promote a legacy String envelope (rendered by a subsystem with its own
/// [`Envelope`]) into the structured result shape. Used at the delegation
/// boundaries (checkpoint compare, coverage provider).
pub fn ok_from_json(json: &str) -> CallToolResult {
    from_envelope_json(json, false)
}

/// Assertion failures are "success" at the transport level but carry
/// `passed: false` so Hermes can branch on the payload rather than the error
/// channel. We still tag category = assertion_failed for clarity.
pub fn err_continued(cat: ErrorCategory, detail: String) -> CallToolResult {
    let env = Envelope {
        category: cat,
        error: Some(detail),
        data: Some(serde_json::json!({ "passed": false })),
    };
    from_envelope_json(&env.to_json(), false)
}

/// Uniform `invalid_request` for an unrecognized selector value: names what
/// was passed and every accepted variant (Wave G item 70 — the agent can
/// self-correct from the message alone).
pub fn err_invalid_selector(
    what: &str,
    got: &crate::mcp::params::Known<impl std::any::Any>,
    variants: &[&'static str],
) -> CallToolResult {
    let shown = match got {
        crate::mcp::params::Known::Other(s) => s.clone(),
        _ => String::new(),
    };
    err(
        ErrorCategory::InvalidRequest,
        format!(
            "unknown {} '{}' (expected one of: {})",
            what,
            shown,
            variants.join(", ")
        ),
    )
}

/// Parse an ergonomic key string into a typed [`KeyEvent`] (spec section 6).
///
/// Modifier names are matched case-insensitively, but a single-character final
/// key *preserves its original case*.  When SHIFT is set on a letter, the
/// character is uppercased (so `shift+a` yields `Char('A')`).  Multi-char
/// unknown names still error.
/// Parse an ergonomic key string ("ctrl+c", "shift+tab") into a typed
/// [`KeyEvent`]. Public wrapper so [`crate::execution::CanonicalAction`]
/// shares the one parser — MCP `tui_act` and any persisted/replayed action
/// must agree on key names.
pub fn parse_key_public(s: &str) -> Result<KeyEvent, String> {
    parse_key(s)
}

fn parse_key(s: &str) -> Result<KeyEvent, String> {
    use crate::backend::KeyCode;
    use KeyModifiers as M;
    let lower = s.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let mut modifiers = M::NONE;
    // Determine modifiers from all but the last part.
    for part in &parts[..parts.len().saturating_sub(1)] {
        match *part {
            "ctrl" | "c" => modifiers |= M::CTRL,
            "alt" | "meta" | "option" => modifiers |= M::ALT,
            "shift" => modifiers |= M::SHIFT,
            "super" | "cmd" | "win" => modifiers |= M::SUPER,
            other => return Err(format!("unknown modifier '{}' in key '{}'", other, s)),
        }
    }
    let key = parts.last().copied().unwrap_or(s);
    // The original (unlowercased) last part — used for single-char keys to
    // preserve the caller's casing.
    let orig_last = s.rsplit_once('+').map_or(s, |(_, rest)| rest);
    let code = match key {
        "enter" | "return" | "\n" | "\r" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "escape" | "esc" => KeyCode::Escape,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "right" => KeyCode::Right,
        "left" => KeyCode::Left,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "f1" => KeyCode::Function(1),
        "f2" => KeyCode::Function(2),
        "f3" => KeyCode::Function(3),
        "f4" => KeyCode::Function(4),
        "f5" => KeyCode::Function(5),
        "f6" => KeyCode::Function(6),
        "f7" => KeyCode::Function(7),
        "f8" => KeyCode::Function(8),
        "f9" => KeyCode::Function(9),
        "f10" => KeyCode::Function(10),
        "f11" => KeyCode::Function(11),
        "f12" => KeyCode::Function(12),
        other => {
            if other.chars().count() == 1 {
                // Single-char: preserve original case, then apply SHIFT.
                let ch = orig_last.chars().next().unwrap();
                let ch = if modifiers.contains(M::SHIFT) && ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else {
                    ch
                };
                KeyCode::Char(ch)
            } else {
                return Err(format!("unknown key '{}'", s));
            }
        }
    };
    Ok(KeyEvent::with_modifiers(code, modifiers))
}

/// Build a wait condition.
///
/// Honours `quiet_ms` for `screen_stable` and `idle` conditions; defaults to
/// 80 ms / 250 ms respectively when `quiet_ms` is `None`. Unknown condition
/// strings (the `Known::Other` case) return `None` — the caller answers
/// `invalid_request` naming the accepted set.
pub fn build_wait(p: &crate::mcp::params::TuiWaitParams) -> Option<crate::backend::WaitCond> {
    use crate::backend::WaitCond;
    use crate::mcp::params::WaitCondition as W;
    let quiet = p
        .quiet_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(80));
    let idle_quiet = p
        .quiet_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(250));
    let cond = p.condition.known()?;
    Some(match cond {
        W::Text => p.text.clone().map(WaitCond::Text)?,
        W::TextAbsent => p.text.clone().map(WaitCond::TextAbsent)?,
        W::ScreenChange => WaitCond::ScreenChange,
        W::ScreenStable => WaitCond::ScreenStable {
            quiet_for: quiet,
            after_screen_seq: None,
        },
        W::ProcessExit => WaitCond::ProcessExit,
        W::Title => p.title.clone().map(WaitCond::Title)?,
        W::Bell => WaitCond::Bell,
        W::Idle => WaitCond::Idle {
            quiet_for: idle_quiet,
            after_output_seq: None,
        },
        // Wave F item 54: shell-integration command waits (OSC 133).
        W::CommandDone => WaitCond::CommandDone {
            after_command_seq: None,
        },
        W::CommandOutput => WaitCond::CommandOutput {
            text: p.text.clone()?,
            after_command_seq: None,
        },
    })
}

/// Run a single assertion against a screen. Returns `(passed, detail,
/// invalid_request)`. The third element is `Some(InvalidRequest)` when the
/// assertion *name* is unknown (the `Known::Other` case) or a required
/// parameter is missing — that must surface as `invalid_request`, not
/// `assertion_failed`, so Hermes does not mistake a typo for a UI failure
/// (spec section 37).
pub fn run_assertion(
    p: &crate::mcp::params::TuiAssertParams,
    screen: &ScreenState,
) -> (bool, String, Option<ErrorCategory>) {
    use crate::mcp::params::AssertAssertion as A;
    let joined = screen.viewport_text.join("\n");
    let Some(a) = p.assertion.known() else {
        return (
            false,
            format!(
                "unknown assertion '{}' (expected one of: {})",
                p.assertion_name(),
                <crate::mcp::params::AssertAssertion as crate::mcp::params::EnumVariants>::VARIANTS
                    .join(", ")
            ),
            Some(ErrorCategory::InvalidRequest),
        );
    };
    match a {
        A::Text => match &p.text {
            Some(t) => (
                joined.contains(t.as_str()),
                format!("expected text '{}'", t),
                None,
            ),
            None => (
                false,
                "text assertion requires 'text'".into(),
                Some(ErrorCategory::InvalidRequest),
            ),
        },
        A::TextAbsent => match &p.text {
            Some(t) => (
                !joined.contains(t.as_str()),
                format!("expected absence of '{}'", t),
                None,
            ),
            None => (
                false,
                "text_absent requires 'text'".into(),
                Some(ErrorCategory::InvalidRequest),
            ),
        },
        A::Focus => match &p.subject {
            Some(s) => {
                let sem = crate::semantic::analyze(screen);
                let ok = sem.focus.control.as_deref() == Some(s.as_str());
                (
                    ok,
                    format!("expected focus on '{}', got {:?}", s, sem.focus.control),
                    None,
                )
            }
            None => (
                false,
                "focus assertion requires 'subject'".into(),
                Some(ErrorCategory::InvalidRequest),
            ),
        },
        A::Dimensions => {
            let c = p.cols.unwrap_or(screen.cols);
            let r = p.rows.unwrap_or(screen.rows);
            let ok = screen.cols == c && screen.rows == r;
            (
                ok,
                format!("expected {}x{}, got {}x{}", c, r, screen.cols, screen.rows),
                None,
            )
        }
        A::ExitCode => {
            if screen.process.running {
                (
                    false,
                    format!(
                        "process still running (expected exit code {:?})",
                        p.expected_code
                    ),
                    None,
                )
            } else {
                match p.expected_code {
                    Some(code) => {
                        let actual = screen.process.exit_code.unwrap_or(-1);
                        let ok = actual == code;
                        (
                            ok,
                            format!("expected exit code {}, got {}", code, actual),
                            None,
                        )
                    }
                    None => (true, "process exited".into(), None),
                }
            }
        }
        A::NotClipped => {
            // heuristic: no region extends beyond terminal bounds (spec 17)
            let sem = crate::semantic::analyze(screen);
            let clipped = sem.regions.iter().any(|rg| {
                let b = &rg.bounds;
                (b.x + b.width) > screen.cols || (b.y + b.height) > screen.rows
            });
            (
                !clipped,
                if clipped {
                    "clipping detected".into()
                } else {
                    "no clipping".into()
                },
                None,
            )
        }
        A::Position => {
            // Assert that text appears at a specific row/column.
            match (&p.text, p.x, p.y) {
                (Some(t), Some(x), Some(y)) => {
                    let row = screen.viewport_text.get(y as usize);
                    let ok = row.is_some_and(|r| {
                        r.get((x as usize)..(x as usize + t.len()))
                            .is_some_and(|s| s == t)
                    });
                    (
                        ok,
                        format!(
                            "expected '{}' at ({},{}), row={:?}",
                            t,
                            x,
                            y,
                            row.map(|r| &r[x as usize..])
                        ),
                        None,
                    )
                }
                _ => (
                    false,
                    "position assertion requires 'text', 'x', and 'y'".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        A::Region => {
            // Assert that a region with the given title exists.
            match &p.subject {
                Some(title) => {
                    let sem = crate::semantic::analyze(screen);
                    let found = sem
                        .regions
                        .iter()
                        .find(|r| r.title.as_deref() == Some(title.as_str()));
                    (
                        found.is_some(),
                        if found.is_some() {
                            format!("region '{}' found", title)
                        } else {
                            format!("no region with title '{}'", title)
                        },
                        None,
                    )
                }
                None => (
                    false,
                    "region assertion requires 'subject' (region title)".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        A::Snapshot => {
            // Assert that the current structure_hash matches a reference.
            match &p.reference {
                Some(expected_hash) => {
                    let ok = &screen.structure_hash == expected_hash;
                    (
                        ok,
                        if ok {
                            "structure hash matches".into()
                        } else {
                            format!(
                                "structure hash mismatch: expected {}, got {}",
                                expected_hash, screen.structure_hash
                            )
                        },
                        None,
                    )
                }
                None => (
                    false,
                    "snapshot assertion requires 'reference' (expected hash)".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        A::Structure => {
            // Assert that the structure hash matches a reference (alias of snapshot).
            match &p.reference {
                Some(expected_hash) => {
                    let ok = &screen.structure_hash == expected_hash;
                    (
                        ok,
                        if ok {
                            "structure hash matches".into()
                        } else {
                            format!(
                                "structure hash mismatch: expected {}, got {}",
                                expected_hash, screen.structure_hash
                            )
                        },
                        None,
                    )
                }
                None => (
                    false,
                    "structure assertion requires 'reference' (expected hash)".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        A::ControlExists => {
            // Assert that a control with the given label exists (case-insensitive).
            match &p.subject {
                Some(s) => {
                    let sem = crate::semantic::analyze(screen);
                    let found = sem.controls.iter().any(|c| c.label.eq_ignore_ascii_case(s));
                    (
                        found,
                        if found {
                            format!("control labeled '{}' found", s)
                        } else {
                            let kinds: Vec<String> = sem
                                .controls
                                .iter()
                                .map(|c| format!("{:?}", c.kind))
                                .collect();
                            format!(
                                "no control labeled '{}'; available: {}",
                                s,
                                kinds.join(", ")
                            )
                        },
                        None,
                    )
                }
                None => (
                    false,
                    "control_exists assertion requires 'subject' (control label)".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        A::FocusedNot => {
            // Assert that focus is NOT on the named control (case-insensitive).
            match &p.subject {
                Some(s) => {
                    let sem = crate::semantic::analyze(screen);
                    let not_focused = sem
                        .focus
                        .control
                        .as_deref()
                        .map(|lbl| !lbl.eq_ignore_ascii_case(s))
                        .unwrap_or(true); // no focus at all = also passes
                    (
                        not_focused,
                        format!(
                            "focused_not: focus is {:?}, expected not '{}'",
                            sem.focus.control, s
                        ),
                        None,
                    )
                }
                None => (
                    false,
                    "focused_not assertion requires 'subject' (control label)".into(),
                    Some(ErrorCategory::InvalidRequest),
                ),
            }
        }
        // `oracle` is evaluated at the MCP layer against the shared Wave E
        // language; if it lands here the dispatch drifted — say so honestly
        // as invalid_request rather than guessing.
        A::Oracle => (
            false,
            "assertion 'oracle' must be evaluated with a live session context; this path should not have been reached".into(),
            Some(ErrorCategory::InvalidRequest),
        ),
    }
}

/// Check whether a control with the given label exists in the screen.
/// Case-insensitive exact match.
///
/// Pure helper suitable for test exposure (helpers.rs is a lib module).
pub fn control_label_exists(screen: &ScreenState, label: &str) -> bool {
    let sem = crate::semantic::analyze(screen);
    sem.controls
        .iter()
        .any(|c| c.label.eq_ignore_ascii_case(label))
}
