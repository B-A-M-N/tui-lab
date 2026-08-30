//! The one canonical executor for interactions (audit re-review item 4).
//!
//! MCP `tui_act`/`tui_wait`/`tui_assert`, the ScenarioRunner, the explorer,
//! and the audit drivers must never have separate interpretations of what
//! `"ctrl+c"`, `screen_stable`, or `focus` mean. They all go through
//! [`execute_act`] / [`execute_wait`] / [`execute_assert`] here, which are
//! the *only* places that translate an intent into Session operations.
//!
//! Every act runs as an [`InteractionTransaction`]: baseline event state is
//! captured *before* the input is sent, the settle wait is anchored on that
//! baseline (`wait_after`), and the outcome reports honestly whether the
//! screen actually settled (re-review items 8/9).

use crate::backend::{CaptureOutcome, Input, TerminalEventState, WaitCond, WaitOutcome};
use crate::screen::diff::Transition;
use crate::screen::ScreenState;
use crate::session::state::Session;

/// The typed, serializable action unit (re-review Wave-2 item 10).
///
/// `execute_act` used to take a bare `(name: &str, input: Input)` pair: the
/// name was display metadata with no relation to the payload, and neither
/// half could persist — the run ledger, scenarios, and the repro minimizer
/// could *count* an action but never reconstruct it (mouse coordinates,
/// typed text, key modifiers were all lost).
///
/// [`CanonicalAction`] closes that gap: it is the one serializable value
/// that (a) translates losslessly to the backend [`Input`], (b) carries a
/// stable `name()` for evidence/display, and (c) round-trips through JSON
/// so scenario replay and repro minimization execute the *same* action the
/// live caller sent. Transport concerns (settle waits, budgets) stay out —
/// those wrap the action, they are not part of it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CanonicalAction {
    /// One key event, modifiers included (`ctrl+c`, `shift+tab`).
    Key {
        key: crate::backend::KeyEvent,
    },
    /// A sequence of key events sent as a unit.
    Keys {
        keys: Vec<crate::backend::KeyEvent>,
    },
    /// Type literal text (as if typed).
    Type {
        text: String,
    },
    /// Paste a (possibly large) block; sensitive payloads are redacted by
    /// recorders, never by this type.
    Paste {
        text: String,
    },
    /// Raw bytes straight to the PTY (escape-sequence-level testing).
    Raw {
        bytes: Vec<u8>,
    },
    /// Full click: backend emits press AND release (audit item 8).
    MouseClick {
        button: crate::backend::MouseButton,
        x: u16,
        y: u16,
    },
    MousePress {
        button: crate::backend::MouseButton,
        x: u16,
        y: u16,
    },
    MouseRelease {
        button: crate::backend::MouseButton,
        x: u16,
        y: u16,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseDrag {
        button: crate::backend::MouseButton,
        x: u16,
        y: u16,
    },
    MouseScroll {
        direction: crate::backend::ScrollDirection,
        x: u16,
        y: u16,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// POSIX signal to the child process group.
    Signal {
        signal: i32,
    },
}

impl CanonicalAction {
    /// Translate to the backend input event. The one conversion; no caller
    /// re-derives an `Input` from parts.
    pub fn to_input(&self) -> Input {
        use crate::backend::MouseEvent;
        match self {
            CanonicalAction::Key { key } => Input::Key(*key),
            CanonicalAction::Keys { keys } => Input::Keys(keys.clone()),
            CanonicalAction::Type { text } => Input::Text(text.clone()),
            CanonicalAction::Paste { text } => Input::Paste(text.clone()),
            CanonicalAction::Raw { bytes } => Input::Raw(bytes.clone()),
            CanonicalAction::MouseClick { button, x, y } => Input::MouseClick {
                button: *button,
                x: *x,
                y: *y,
            },
            CanonicalAction::MousePress { button, x, y } => Input::Mouse(MouseEvent::Press {
                button: *button,
                x: *x,
                y: *y,
            }),
            CanonicalAction::MouseRelease { button, x, y } => Input::Mouse(MouseEvent::Release {
                button: *button,
                x: *x,
                y: *y,
            }),
            CanonicalAction::MouseMove { x, y } => Input::Mouse(MouseEvent::Move { x: *x, y: *y }),
            CanonicalAction::MouseDrag { button, x, y } => Input::Mouse(MouseEvent::Drag {
                button: *button,
                x: *x,
                y: *y,
            }),
            CanonicalAction::MouseScroll { direction, x, y } => Input::Mouse(MouseEvent::Scroll {
                direction: *direction,
                x: *x,
                y: *y,
            }),
            CanonicalAction::Resize { cols, rows } => Input::Resize {
                cols: *cols,
                rows: *rows,
            },
            CanonicalAction::Signal { signal } => Input::Signal(*signal),
        }
    }

    /// Stable display/evidence name, matching the action names already used
    /// across the ledger, explorer, and audits.
    pub fn name(&self) -> &'static str {
        match self {
            CanonicalAction::Key { .. } => "key",
            CanonicalAction::Keys { .. } => "keys",
            CanonicalAction::Type { .. } => "type",
            CanonicalAction::Paste { .. } => "paste",
            CanonicalAction::Raw { .. } => "raw",
            CanonicalAction::MouseClick { .. } => "mouse_click",
            CanonicalAction::MousePress { .. } => "mouse_press",
            CanonicalAction::MouseRelease { .. } => "mouse_release",
            CanonicalAction::MouseMove { .. } => "mouse_move",
            CanonicalAction::MouseDrag { .. } => "mouse_drag",
            CanonicalAction::MouseScroll { .. } => "mouse_scroll",
            CanonicalAction::Resize { .. } => "resize",
            CanonicalAction::Signal { .. } => "signal",
        }
    }

    /// From the MCP request shape. Transport fields (`no_wait`, `wait_ms`,
    /// `id`) are deliberately dropped — they describe *how to observe* the
    /// action, not the action itself.
    pub fn from_request(req: &crate::mcp::params::TuiActRequest) -> Result<Self, String> {
        use crate::backend::{MouseButton as MB, ScrollDirection as SD};
        use crate::mcp::params::TuiActRequest as R;
        let btn =
            |b: &Option<crate::mcp::params::MouseButtonParam>| b.map(MB::from).unwrap_or(MB::Left);
        match req {
            R::Key { key, .. } => Ok(CanonicalAction::Key {
                key: crate::mcp::helpers::parse_key_public(key)?,
            }),
            R::Keys { keys, .. } => {
                if keys.is_empty() {
                    return Err("empty keys".into());
                }
                let mut out = Vec::with_capacity(keys.len());
                for k in keys {
                    out.push(crate::mcp::helpers::parse_key_public(k)?);
                }
                Ok(CanonicalAction::Keys { keys: out })
            }
            R::Type { text, .. } => Ok(CanonicalAction::Type { text: text.clone() }),
            R::Paste { paste, .. } => Ok(CanonicalAction::Paste {
                text: paste.clone(),
            }),
            R::Raw { raw, .. } => {
                if raw.is_empty() {
                    return Err("empty raw payload".into());
                }
                Ok(CanonicalAction::Raw { bytes: raw.clone() })
            }
            R::MouseClick { x, y, button, .. } => Ok(CanonicalAction::MouseClick {
                button: btn(button),
                x: *x,
                y: *y,
            }),
            R::MousePress { x, y, button, .. } => Ok(CanonicalAction::MousePress {
                button: btn(button),
                x: *x,
                y: *y,
            }),
            R::MouseRelease { x, y, button, .. } => Ok(CanonicalAction::MouseRelease {
                button: btn(button),
                x: *x,
                y: *y,
            }),
            R::MouseMove { x, y, .. } => Ok(CanonicalAction::MouseMove { x: *x, y: *y }),
            R::MouseDrag { x, y, button, .. } => Ok(CanonicalAction::MouseDrag {
                button: btn(button),
                x: *x,
                y: *y,
            }),
            R::MouseScroll {
                x, y, direction, ..
            } => Ok(CanonicalAction::MouseScroll {
                direction: direction.map(SD::from).unwrap_or(SD::Down),
                x: *x,
                y: *y,
            }),
            R::Resize { cols, rows, .. } => Ok(CanonicalAction::Resize {
                cols: *cols,
                rows: *rows,
            }),
            R::Signal { signal, .. } => Ok(CanonicalAction::Signal { signal: *signal }),
        }
    }
}

/// A named anchor captured *before* an input is sent, used to express
/// "wait for screen change *following this anchor*" rather than "wait for
/// any change". Re-review P0 first-class anchor type.
///
/// Wraps [`TerminalEventState`] (the raw counters) and adds optional
/// metadata for replay/audit evidence. Serialize/deserialize so anchors
/// can be persisted alongside the transactions they anchor.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ObservationAnchor {
    /// Raw counters at the anchor instant.
    pub state: TerminalEventState,
    /// Optional human-readable label (e.g. "before Tab #3").
    pub label: Option<String>,
    /// Monotonic count of anchors captured so far (for ordering).
    pub index: u64,
}

/// One settled interaction: what was sent, whether the screen settled, and
/// the before/after transition. MCP, scenarios, audits, and exploration all
/// consume this shape rather than improvising their own.
///
/// `after` is the authoritative settled frame (re-review P0 "matching frame"):
/// the screen state captured when `wait_after` satisfied its condition, NOT a
/// separate `observe()` call afterwards. Separate post-capture is available
/// via `InteractionTransaction::after_post`.
#[derive(Debug, Clone)]
pub struct InteractionTransaction {
    /// The action name (e.g. "key", "type", "mouse_click").
    pub action: String,
    /// The typed action itself (re-review Wave-2 item 10): serializable, so
    /// the ledger/scenarios can reconstruct exactly what was sent.
    pub canonical: Option<CanonicalAction>,
    /// Whether the post-action settle wait actually met its condition.
    pub settled: bool,
    /// Why the settle resolved (or timed out).
    pub settle_reason: Option<String>,
    pub before: ScreenState,
    /// The frame that satisfied the wait — the authoritative after.
    pub after: ScreenState,
    /// Optional post-settle capture, for callers that want a stable view
    /// after the matching frame (e.g. for diff UI). `None` when not captured.
    pub after_post: Option<ScreenState>,
    pub transition: Transition,
    pub elapsed_ms: u64,
    /// Screen sequence number of the matching after-frame, if known.
    pub after_screen_seq: Option<u64>,
    /// The anchor that scoped the settle wait (re-review P0). Captured
    /// before the input was sent.
    pub anchor: Option<ObservationAnchor>,
    /// Optional capture outcome (re-review P0). When present, this is the
    /// unified result shape for waits/actions/scenarios/audits/replay.
    pub capture: Option<CaptureOutcome>,
}

/// Execute one act against a session with proper causality:
///
/// 1. capture the event baseline *before* sending;
/// 2. send the input;
/// 3. wait for a screen stable *anchored after the baseline* (closes the
///    entry-pump race — re-review item 8);
/// 4. report `settled: false` with the real reason when stabilization timed
///    out instead of silently continuing (re-review item 9).
///
/// `quiet_ms` is the quiet interval that defines "settled" (default 150ms).
/// `settle_budget_ms` bounds the wait (default quiet + 1000ms).
pub fn execute_act(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
) -> Result<InteractionTransaction, anyhow::Error> {
    let before = match session.last().cloned() {
        Some(s) => s,
        None => session.observe(0)?,
    };
    let baseline = session.event_state();
    let anchor = ObservationAnchor {
        state: baseline,
        label: None,
        index: 0, // caller may overwrite via the public field if needed
    };

    session.send(action.to_input())?;

    let (settled, settle_reason, elapsed_ms, after, after_screen_seq, capture) = if no_wait {
        // Even with no_wait, capture a fresh frame so callers always get
        // both before and after. No settle semantics.
        let s = session.observe(quiet_ms)?;
        (true, Some("no_wait".to_string()), 0, s, None, None)
    } else {
        let budget = settle_budget_ms.max(quiet_ms.saturating_add(1000));
        // Use the matching frame from wait_after (re-review P0). No second
        // observe() — the captured state IS the authoritative after.
        let outcome = session.wait_after(
            baseline,
            WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(quiet_ms),
                after_screen_seq: None, // wait_after fills the anchor
            },
            budget,
        )?;
        let capture = CaptureOutcome::from_wait(outcome.clone());
        let reason = format!("{:?}", outcome.reason);
        (
            outcome.met,
            Some(reason),
            outcome.elapsed_ms,
            outcome.state,
            Some(outcome.screen_seq),
            Some(capture),
        )
    };

    let transition = crate::screen::diff(&before, &after);

    Ok(InteractionTransaction {
        action: action.name().to_string(),
        canonical: Some(action.clone()),
        settled,
        settle_reason,
        before,
        after,
        after_post: None,
        transition,
        elapsed_ms,
        after_screen_seq,
        anchor: Some(anchor),
        capture,
    })
}

/// Execute one wait. Thin by design: the point is that every caller uses the
/// same `WaitCond` construction and budget semantics.
pub fn execute_wait(
    session: &mut Session,
    cond: WaitCond,
    budget_ms: u64,
) -> Result<WaitOutcome, anyhow::Error> {
    session.wait(cond, budget_ms)
}

/// Execute one assertion via the shared assertion runner. The MCP layer and
/// the ScenarioRunner must agree on what `focus` or `exit_code` means.
pub fn execute_assert(
    params: &crate::mcp::params::TuiAssertParams,
    screen: &ScreenState,
) -> (bool, String, Option<crate::error::ErrorCategory>) {
    crate::mcp::helpers::run_assertion(params, screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // Lightweight session over a real python child; PTY machinery is
        // exercised in the conformance suites, here we need the sequencing.
        let id = "exec-test".to_string();
        let mut s = Session::new(id, "python3".to_string());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('go'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        s
    }

    #[test]
    fn execute_act_reports_settled_transition() {
        let mut s = session();
        let tx = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
            },
            60,
            1000,
            false,
        )
        .expect("execute act");
        assert!(
            tx.settled,
            "simple echo app must settle: {:?}",
            tx.settle_reason
        );
        assert_eq!(tx.action, "key");
        // echoed char must appear in the after frame
        assert!(
            tx.after.viewport_text.iter().any(|r| r.contains('x')),
            "typed char must be echoed"
        );
    }

    #[test]
    fn execute_act_no_wait_skips_settle() {
        let mut s = session();
        let tx = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('y')),
            },
            60,
            1000,
            true,
        )
        .expect("execute act");
        assert!(tx.settled);
        assert_eq!(tx.settle_reason.as_deref(), Some("no_wait"));
        assert_eq!(tx.elapsed_ms, 0);
    }

    // ── CanonicalAction (Wave-2 item 10) ──────────────────────────────

    /// JSON round-trip preserves every payload byte: replay must execute
    /// the same action the caller sent, including modifiers and coords.
    #[test]
    fn canonical_action_json_roundtrip() {
        use crate::backend::{KeyCode, KeyModifiers, MouseButton, ScrollDirection};
        let actions = vec![
            CanonicalAction::Key {
                key: crate::backend::KeyEvent::with_modifiers(
                    KeyCode::Char('c'),
                    KeyModifiers::CTRL,
                ),
            },
            CanonicalAction::Keys {
                keys: vec![
                    crate::backend::KeyEvent::new(KeyCode::Tab),
                    crate::backend::KeyEvent::new(KeyCode::Enter),
                ],
            },
            CanonicalAction::Type {
                text: "hello 世界".into(),
            },
            CanonicalAction::Paste {
                text: "block\nof text".into(),
            },
            CanonicalAction::Raw {
                bytes: vec![0x1b, 0x5b, 0x41],
            },
            CanonicalAction::MouseClick {
                button: MouseButton::Right,
                x: 12,
                y: 34,
            },
            CanonicalAction::MouseDrag {
                button: MouseButton::Left,
                x: 1,
                y: 2,
            },
            CanonicalAction::MouseScroll {
                direction: ScrollDirection::Up,
                x: 5,
                y: 6,
            },
            CanonicalAction::Resize {
                cols: 120,
                rows: 40,
            },
            CanonicalAction::Signal { signal: 9 },
        ];
        for a in actions {
            let json = serde_json::to_string(&a).expect("serialize");
            let back: CanonicalAction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, a, "round-trip must be lossless for {:?}", a.name());
        }
    }

    /// Names match the action vocabulary already used across the ledger and
    /// audits; `to_input` translates losslessly.
    #[test]
    fn canonical_action_names_and_inputs() {
        use crate::backend::{Input, KeyCode, MouseButton};
        let click = CanonicalAction::MouseClick {
            button: MouseButton::Middle,
            x: 7,
            y: 8,
        };
        assert_eq!(click.name(), "mouse_click");
        match click.to_input() {
            Input::MouseClick { button, x, y } => {
                assert_eq!(button, MouseButton::Middle);
                assert_eq!((x, y), (7, 8));
            }
            _ => panic!("expected MouseClick input"),
        }

        let key = CanonicalAction::Key {
            key: crate::backend::KeyEvent::new(KeyCode::Tab),
        };
        assert_eq!(key.name(), "key");
        assert!(matches!(key.to_input(), Input::Key(_)));
    }

    /// `from_request` captures the full payload and deliberately drops
    /// transport fields; validation errors mirror the old builder.
    #[test]
    fn canonical_action_from_request() {
        use crate::mcp::params::TuiActRequest;
        let req = TuiActRequest::MouseClick {
            x: 3,
            y: 4,
            button: None,
            no_wait: Some(true),
            wait_ms: Some(500),
            id: Some("s1".into()),
        };
        let a = CanonicalAction::from_request(&req).expect("click");
        assert_eq!(a.name(), "mouse_click");
        match a {
            CanonicalAction::MouseClick { button, x, y } => {
                assert_eq!(button, crate::backend::MouseButton::Left, "default button");
                assert_eq!((x, y), (3, 4));
            }
            _ => panic!("wrong variant"),
        }

        let bad = TuiActRequest::Keys {
            keys: vec![],
            no_wait: None,
            wait_ms: None,
            id: None,
        };
        assert!(
            CanonicalAction::from_request(&bad).is_err(),
            "empty keys rejected"
        );

        let raw = TuiActRequest::Raw {
            raw: vec![],
            no_wait: None,
            wait_ms: None,
            id: None,
        };
        assert!(
            CanonicalAction::from_request(&raw).is_err(),
            "empty raw rejected"
        );
    }
}
