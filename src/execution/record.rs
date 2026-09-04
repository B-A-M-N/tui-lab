//! The canonical action vocabulary and its persistence policy.
//!
//! Split from the former single-file `execution` (review §15 god-object
//! residue): [`CanonicalAction`] is the one serializable value every
//! caller (MCP, scenarios, repro minimizer) sends; [`InputVisibility`] and
//! [`PersistedAction`] govern what may reach a durable artifact;
//! [`ObservationAnchor`] is the pre-input event baseline the settle wait
//! anchors on.

use crate::backend::{Input, TerminalEventState};

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

    /// EXACT provenance for evidence graphs (re-review P0): what was this
    /// action, specifically — `tab`, `shift+tab`, `ctrl+c`, `down`,
    /// `mouse:left:click@12,3`, `resize:80x24`, `signal:9`. `name()` says
    /// the KIND; this says the IDENTITY, so a focus edge can read "moved
    /// via tab" instead of the useless "via key".
    pub fn signature(&self) -> String {
        fn mods(m: crate::backend::KeyModifiers) -> String {
            let mut out = String::new();
            if m.contains(crate::backend::KeyModifiers::CTRL) {
                out.push_str("ctrl+");
            }
            if m.contains(crate::backend::KeyModifiers::ALT) {
                out.push_str("alt+");
            }
            if m.contains(crate::backend::KeyModifiers::SHIFT) {
                out.push_str("shift+");
            }
            if m.contains(crate::backend::KeyModifiers::SUPER) {
                out.push_str("super+");
            }
            out
        }
        fn key_name(k: &crate::backend::KeyEvent) -> String {
            use crate::backend::KeyCode as C;
            let base = match &k.code {
                C::Char(c) => c.to_string(),
                C::Enter => "enter".into(),
                C::Escape => "escape".into(),
                C::Tab => "tab".into(),
                C::Backspace => "backspace".into(),
                C::Up => "up".into(),
                C::Down => "down".into(),
                C::Left => "left".into(),
                C::Right => "right".into(),
                C::Home => "home".into(),
                C::End => "end".into(),
                C::PageUp => "pageup".into(),
                C::PageDown => "pagedown".into(),
                C::Insert => "insert".into(),
                C::Delete => "delete".into(),
                C::Function(n) => format!("f{n}"),
            };
            format!("{}{}", mods(k.modifiers), base)
        }
        fn button_name(b: crate::backend::MouseButton) -> &'static str {
            match b {
                crate::backend::MouseButton::Left => "left",
                crate::backend::MouseButton::Middle => "middle",
                crate::backend::MouseButton::Right => "right",
            }
        }
        match self {
            CanonicalAction::Key { key } => key_name(key),
            CanonicalAction::Keys { keys } => {
                keys.iter().map(key_name).collect::<Vec<_>>().join("+")
            }
            CanonicalAction::Type { .. } => "text".into(),
            CanonicalAction::Paste { .. } => "paste".into(),
            CanonicalAction::Raw { .. } => "raw".into(),
            CanonicalAction::MouseClick { button, x, y } => {
                format!("mouse:{}:click@{x},{y}", button_name(*button))
            }
            CanonicalAction::MousePress { button, x, y } => {
                format!("mouse:{}:press@{x},{y}", button_name(*button))
            }
            CanonicalAction::MouseRelease { button, x, y } => {
                format!("mouse:{}:release@{x},{y}", button_name(*button))
            }
            CanonicalAction::MouseMove { x, y } => format!("mouse:move@{x},{y}"),
            CanonicalAction::MouseDrag { button, x, y } => {
                format!("mouse:{}:drag@{x},{y}", button_name(*button))
            }
            CanonicalAction::MouseScroll { direction, x, y } => {
                let d = match direction {
                    crate::backend::ScrollDirection::Up => "up",
                    crate::backend::ScrollDirection::Down => "down",
                };
                format!("mouse:wheel:{d}@{x},{y}")
            }
            CanonicalAction::Resize { cols, rows } => format!("resize:{cols}x{rows}"),
            CanonicalAction::Signal { signal } => format!("signal:{signal}"),
        }
    }

    /// Byte length of the payload this action carries (leak-fix support for
    /// redacted persistence: the length is replay-relevant metadata and is
    /// safe to keep; the bytes are not).
    pub fn payload_len(&self) -> usize {
        match self {
            CanonicalAction::Type { text } | CanonicalAction::Paste { text } => text.len(),
            CanonicalAction::Raw { bytes } => bytes.len(),
            _ => 0,
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

/// Settlement outcome for one interaction (re-review P1: `no_wait` must not
/// report `settled` — "I did not test settlement" is not "it settled").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleStatus {
    /// The anchored settle condition was met within budget.
    Met,
    /// The settle condition timed out.
    TimedOut,
    /// Settlement was not tested (`no_wait=true`).
    Skipped,
}

impl SettleStatus {
    /// Bridge from the legacy `(settled, reason)` pair while callers migrate.
    /// The executor's no-wait path reports reason `"no_wait"`.
    pub fn from_legacy(settled: bool, reason: &str) -> Self {
        if reason == "no_wait" {
            SettleStatus::Skipped
        } else if settled {
            SettleStatus::Met
        } else {
            SettleStatus::TimedOut
        }
    }
}

/// How an input payload may be persisted (re-review P0 leak fix). This is a
/// property of the action envelope, not of the MCP request: every recorder
/// (transaction ledger, scenarios, traces, recordings, error/debug output)
/// consults it, so a sensitive payload can never survive through a path that
/// forgot to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputVisibility {
    /// Payload may be persisted verbatim.
    Normal,
    /// Payload must never appear verbatim in any artifact; a redacted
    /// stand-in (kind + byte length) is persisted instead.
    Sensitive,
    /// Same guarantee as [`InputVisibility::Sensitive`]; reserved for an
    /// explicit never-persist request so the two intents stay distinguishable
    /// in evidence.
    NeverPersist,
}

/// The persistence-boundary view of a [`CanonicalAction`]: either the full
/// action, or a redacted stand-in that keeps replay-relevant metadata (kind,
/// payload byte length) without the payload. The live executor always
/// receives the real action — redaction happens only at persistence.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "persistence", rename_all = "snake_case")]
pub enum PersistedAction {
    Full(CanonicalAction),
    Redacted {
        /// Action kind name (`type`, `paste`, ...).
        kind: String,
        /// Byte length of the redacted payload.
        byte_len: usize,
    },
}

impl PersistedAction {
    /// Project an action through a visibility policy. Only text-carrying
    /// actions (`type`, `paste`) redact — key events, coordinates, and raw
    /// control sequences carry no user-secret payload.
    pub fn project(action: &CanonicalAction, visibility: InputVisibility) -> Self {
        match visibility {
            InputVisibility::Normal => PersistedAction::Full(action.clone()),
            InputVisibility::Sensitive | InputVisibility::NeverPersist => match action {
                CanonicalAction::Type { text } | CanonicalAction::Paste { text } => {
                    PersistedAction::Redacted {
                        kind: action.name().to_string(),
                        byte_len: text.len(),
                    }
                }
                other => PersistedAction::Full(other.clone()),
            },
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
