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

    /// Replay-safe payload text for a Type action: the text is itself
    /// semantic input, so a recorded precondition should require its
    /// presence rather than pin the volatile fused identity it changes.
    /// `None` for non-Type actions.
    pub fn payload_text(&self) -> Option<String> {
        match self {
            CanonicalAction::Type { text } => Some(text.clone()),
            _ => None,
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
            R::Key(p) => Ok(CanonicalAction::Key {
                key: crate::mcp::helpers::parse_key_public(&p.key)?,
            }),
            R::Keys(p) => {
                let keys = &p.keys;
                if keys.is_empty() {
                    return Err("empty keys".into());
                }
                let mut out = Vec::with_capacity(keys.len());
                for k in keys {
                    out.push(crate::mcp::helpers::parse_key_public(k)?);
                }
                Ok(CanonicalAction::Keys { keys: out })
            }
            R::Type(p) => Ok(CanonicalAction::Type {
                text: p.text.clone(),
            }),
            R::Paste(p) => Ok(CanonicalAction::Paste {
                text: p.paste.clone(),
            }),
            R::Raw(p) => {
                let raw = &p.raw;
                if raw.is_empty() {
                    return Err("empty raw payload".into());
                }
                Ok(CanonicalAction::Raw { bytes: raw.clone() })
            }
            R::MouseClick(p) => Ok(CanonicalAction::MouseClick {
                button: btn(&p.button),
                x: p.x,
                y: p.y,
            }),
            R::MousePress(p) => Ok(CanonicalAction::MousePress {
                button: btn(&p.button),
                x: p.x,
                y: p.y,
            }),
            R::MouseRelease(p) => Ok(CanonicalAction::MouseRelease {
                button: btn(&p.button),
                x: p.x,
                y: p.y,
            }),
            R::MouseMove(p) => Ok(CanonicalAction::MouseMove { x: p.x, y: p.y }),
            R::MouseDrag(p) => Ok(CanonicalAction::MouseDrag {
                button: btn(&p.button),
                x: p.x,
                y: p.y,
            }),
            R::MouseScroll(p) => Ok(CanonicalAction::MouseScroll {
                direction: p.direction.map(SD::from).unwrap_or(SD::Down),
                x: p.x,
                y: p.y,
            }),
            R::Resize(p) => Ok(CanonicalAction::Resize {
                cols: p.cols,
                rows: p.rows,
            }),
            R::Signal(p) => Ok(CanonicalAction::Signal { signal: p.signal }),
        }
    }
}

/// Which subsystem drove one interaction (audit finding 2: one driving
/// authority, with provenance). Every act lands in the same canonical
/// executor and the same run ledger; the origin is the typed answer to
/// "WHO sent this input" — `tui_act`, a scenario replay, the explorer,
/// an audit driver — recorded per transaction instead of being
/// reconstructable only from context. The scenario runner, exploration,
/// audit drivers, conformance, and the diagnostic probe all tag their
/// acts; the ledger row carries the slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveOrigin {
    /// The `tui_act` MCP tool — the agent's direct request.
    Act,
    /// An executed intent plan (`tui_intent execute`), including its
    /// internal focus-move steps.
    Intent,
    /// Scenario replay (tui_scenario run / recording playback).
    Scenario,
    /// Random exploration.
    Explore,
    /// Reproduction replay (crash minimization candidate).
    Repro,
    /// Semantic/exploration candidate driving.
    ExploreSemantic,
    /// An audit driver (keyboard, states, layout, interaction, ...).
    Audit,
    /// Backend/design conformance probing.
    Conformance,
    /// The diagnostic probe (tui_probe stimulus path).
    Probe,
    /// The contract scaffold's multi-state gather pass (finding 37):
    /// Tab/Escape/resize steps recorded while building a scaffold.
    Scaffold,
}

impl DriveOrigin {
    /// The ledger/wire slug.
    pub fn as_str(&self) -> &'static str {
        match self {
            DriveOrigin::Act => "act",
            DriveOrigin::Intent => "intent",
            DriveOrigin::Scenario => "scenario",
            DriveOrigin::Explore => "explore",
            DriveOrigin::Repro => "repro",
            DriveOrigin::ExploreSemantic => "explore_semantic",
            DriveOrigin::Audit => "audit",
            DriveOrigin::Conformance => "conformance",
            DriveOrigin::Probe => "probe",
            DriveOrigin::Scaffold => "scaffold",
        }
    }
}

/// Exact write-boundary state for one logical dispatch. A guard refusal
/// never reaches the transport; a transport failure after bytes are known
/// representable is at least unknown-partial and must be evidenced rather
/// than disappearing through `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStatus {
    /// The action was refused before any transport write.
    RefusedBeforeWrite,
    /// The backend reported success after its complete payload write.
    Sent,
    /// The transport failed before any bytes were known to be written.
    FailedBeforeWrite,
    /// The transport failed at a point where partial delivery cannot be
    /// ruled out. Evidence must record this explicitly.
    PartialOrUnknown,
}

impl DispatchStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::RefusedBeforeWrite => "refused_before_write",
            Self::Sent => "sent",
            Self::FailedBeforeWrite => "failed_before_write",
            Self::PartialOrUnknown => "partial_or_unknown",
        }
    }

    /// Whether a write was attempted for this status. `Sent` and
    /// `PartialOrUnknown` both imply the transport write was entered;
    /// the two pre-write statuses prove no bytes could have landed.
    pub fn write_attempted(&self) -> bool {
        matches!(self, Self::Sent | Self::PartialOrUnknown)
    }
}

/// The typed outcome of one transport dispatch (audit finding 4): the
/// backend's original error is preserved, plus the exact classification of
/// whether the write was attempted. The executor maps this onto
/// [`DispatchStatus`] without collapsing distinct failure classes into
/// `PartialOrUnknown`.
#[derive(Debug, Clone)]
pub struct DispatchError {
    pub status: DispatchStatus,
    /// The backend's original error string (BackendError or resize error),
    /// preserved verbatim for evidence.
    pub message: String,
}

impl DispatchError {
    /// Classify a send/resize failure by what the backend proved. A
    /// preflight/encoding/unsupported failure proves no bytes were written;
    /// anything observed only as a transport I/O error after the write
    /// boundary is entered is `PartialOrUnknown` unless the backend proved
    /// otherwise.
    pub fn from_backend(status: DispatchStatus, message: String) -> Self {
        DispatchError { status, message }
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status.name(), self.message)
    }
}

impl std::error::Error for DispatchError {}

/// A typed [`Result`] for transport dispatch.
pub type DispatchResult<T> = Result<T, DispatchError>;

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
    /// No settle wait was attempted — the dispatch failed before (or
    /// with unknown) delivery, so settlement is not merely untested, it
    /// was never reachable. Distinct from `Skipped` (`no_wait`: we
    /// CHOSE not to wait on a sent input) and from `TimedOut` (a wait
    /// ran and exhausted its budget).
    NotAttempted,
}

impl SettleStatus {
    /// Bridge from the legacy `(settled, reason)` pair while callers migrate.
    /// The executor's no-wait path reports reason `"no_wait"`.
    pub fn from_legacy(settled: bool, reason: &str) -> Self {
        if reason == "no_wait" {
            SettleStatus::Skipped
        } else if reason == "dispatch_failed" {
            SettleStatus::NotAttempted
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
