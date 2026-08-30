//! Shared helpers for the MCP tools layer: envelope rendering, input/wait
//! builders, assertion runner, cast writer, and small global stores.

use crate::error::{Envelope, ErrorCategory};
use crate::screen::ScreenState;
use std::time::Duration;

use crate::backend::{KeyEvent, KeyModifiers, MouseButton};

/// Render a success envelope.
pub fn ok(data: serde_json::Value) -> String {
    let env = Envelope::ok(data);
    env.to_json()
}

/// Render a hard-failure envelope (category + message).
pub fn err(cat: ErrorCategory, msg: impl Into<String>) -> String {
    let env: Envelope<()> = Envelope::<()>::fail(cat, msg);
    env.to_json()
}

/// Assertion failures are "success" at the transport level but carry
/// `passed: false` so Hermes can branch on the payload rather than the error
/// channel. We still tag category = assertion_failed for clarity.
pub fn err_continued(cat: ErrorCategory, detail: String) -> String {
    let env = Envelope {
        category: cat,
        error: Some(detail),
        data: Some(serde_json::json!({ "passed": false })),
    };
    env.to_json()
}

/// Build a [`crate::backend::Input`] from a tagged [`crate::mcp::params::TuiActRequest`].
pub fn build_input_from_request(
    p: &crate::mcp::params::TuiActRequest,
) -> Result<crate::backend::Input, String> {
    use crate::backend::{Input, MouseEvent, ScrollDirection};
    match p {
        crate::mcp::params::TuiActRequest::Key { key, .. } => Ok(Input::Key(parse_key(key)?)),
        crate::mcp::params::TuiActRequest::Keys { keys, .. } => {
            if keys.is_empty() {
                return Err("empty keys".into());
            }
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(parse_key(k)?);
            }
            Ok(Input::Keys(out))
        }
        crate::mcp::params::TuiActRequest::Type { text, .. } => Ok(Input::Text(text.clone())),
        crate::mcp::params::TuiActRequest::Paste { paste, .. } => Ok(Input::Paste(paste.clone())),
        crate::mcp::params::TuiActRequest::Raw { raw, .. } => {
            if raw.is_empty() {
                return Err("empty raw payload".into());
            }
            Ok(Input::Raw(raw.clone()))
        }
        crate::mcp::params::TuiActRequest::MouseClick { x, y, button, .. } => {
            Ok(Input::MouseClick {
                button: button.map(crate::backend::MouseButton::from).unwrap_or(MouseButton::Left),
                x: *x,
                y: *y,
            })
        }
        crate::mcp::params::TuiActRequest::MousePress { x, y, button, .. } => {
            Ok(Input::Mouse(MouseEvent::Press {
                button: button.map(crate::backend::MouseButton::from).unwrap_or(MouseButton::Left),
                x: *x,
                y: *y,
            }))
        }
        crate::mcp::params::TuiActRequest::MouseRelease { x, y, button, .. } => {
            Ok(Input::Mouse(MouseEvent::Release {
                button: button.map(crate::backend::MouseButton::from).unwrap_or(MouseButton::Left),
                x: *x,
                y: *y,
            }))
        }
        crate::mcp::params::TuiActRequest::MouseMove { x, y, .. } => {
            Ok(Input::Mouse(MouseEvent::Move { x: *x, y: *y }))
        }
        crate::mcp::params::TuiActRequest::MouseDrag { x, y, button, .. } => {
            Ok(Input::Mouse(MouseEvent::Drag {
                button: button.map(crate::backend::MouseButton::from).unwrap_or(MouseButton::Left),
                x: *x,
                y: *y,
            }))
        }
        crate::mcp::params::TuiActRequest::MouseScroll {
            x, y, direction, ..
        } => Ok(Input::Mouse(MouseEvent::Scroll {
            direction: direction
                .map(crate::backend::ScrollDirection::from)
                .unwrap_or(ScrollDirection::Down),
            x: *x,
            y: *y,
        })),
        crate::mcp::params::TuiActRequest::Resize { cols, rows, .. } => Ok(Input::Resize {
            cols: *cols,
            rows: *rows,
        }),
        crate::mcp::params::TuiActRequest::Signal { signal, .. } => Ok(Input::Signal(*signal)),
    }
}

/// Parse an ergonomic key string into a typed [`KeyEvent`] (spec section 6).
///
/// Modifier names are matched case-insensitively, but a single-character final
/// key *preserves its original case*.  When SHIFT is set on a letter, the
/// character is uppercased (so `shift+a` yields `Char('A')`).  Multi-char
/// unknown names still error.
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
/// 80 ms / 250 ms respectively when `quiet_ms` is `None`.
pub fn build_wait(p: &crate::mcp::params::TuiWaitParams) -> Option<crate::backend::WaitCond> {
    use crate::backend::WaitCond;
    let quiet = p
        .quiet_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(80));
    let idle_quiet = p
        .quiet_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(250));
    match p.condition.as_str() {
        "text" => p.text.clone().map(WaitCond::Text),
        "text_absent" => p.text.clone().map(WaitCond::TextAbsent),
        "screen_change" => Some(WaitCond::ScreenChange),
        "screen_stable" => Some(WaitCond::ScreenStable {
            quiet_for: quiet,
            after_screen_seq: None,
        }),
        "process_exit" => Some(WaitCond::ProcessExit),
        "title" => p.title.clone().map(WaitCond::Title),
        "bell" => Some(WaitCond::Bell),
        "idle" => Some(WaitCond::Idle {
            quiet_for: idle_quiet,
            after_output_seq: None,
        }),
        _ => None,
    }
}

/// Run a single assertion against a screen. Returns `(passed, detail,
/// invalid_request)`. The third element is `Some(InvalidRequest)` when the
/// assertion *name* is unknown — that must surface as `invalid_request`, not
/// `assertion_failed`, so Hermes does not mistake a typo for a UI failure
/// (spec section 37).
pub fn run_assertion(
    p: &crate::mcp::params::TuiAssertParams,
    screen: &ScreenState,
) -> (bool, String, Option<ErrorCategory>) {
    let joined = screen.viewport_text.join("\n");
    match p.assertion.as_str() {
        "text" => match &p.text {
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
        "text_absent" => match &p.text {
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
        "focus" => match &p.subject {
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
        "dimensions" => {
            let c = p.cols.unwrap_or(screen.cols);
            let r = p.rows.unwrap_or(screen.rows);
            let ok = screen.cols == c && screen.rows == r;
            (
                ok,
                format!("expected {}x{}, got {}x{}", c, r, screen.cols, screen.rows),
                None,
            )
        }
        "exit_code" => {
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
        "not_clipped" => {
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
        "position" => {
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
        "region" => {
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
        "snapshot" => {
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
        "structure" => {
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
        "control_exists" => {
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
        "focused_not" => {
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
        other => (
            false,
            format!("unknown assertion '{}'", other),
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
    sem.controls.iter().any(|c| c.label.eq_ignore_ascii_case(label))
}

/// Write an asciinema v3 (NDJSON) event list from a screen snapshot.
pub fn write_cast(screen: &ScreenState) -> Vec<String> {
    let mut events = Vec::new();
    // header event
    events.push(
        serde_json::json!({
            "version": 3,
            "width": screen.cols,
            "height": screen.rows,
            "timestamp": 0,
        })
        .to_string(),
    );
    events.push(
        serde_json::json!({
            "time": 0,
            "type": "o",
            "data": screen.viewport_text.join("\n"),
        })
        .to_string(),
    );
    events
}

/// LEGACY (pre-RunContext): scheduled for removal in the MCP rewiring wave.
///
/// Global checkpoint/store maps (OnceLock + Mutex). Defined here as thread-safe
/// process-wide singletons; the server serializes calls (no parallel tool calls).
use std::sync::OnceLock;
pub static CHECKPOINTS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
    OnceLock::new();
pub static SCENARIOS: OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>,
> = OnceLock::new();

/// LEGACY (pre-RunContext): scheduled for removal in the MCP rewiring wave.
pub fn checkpoints() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    CHECKPOINTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
/// LEGACY (pre-RunContext): scheduled for removal in the MCP rewiring wave.
pub fn scenarios(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>> {
    SCENARIOS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
