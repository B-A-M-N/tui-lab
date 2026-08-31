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

use crate::backend::{
    CanonicalFrame, CaptureOutcome, Input, TerminalEventState, WaitCond, WaitOutcome,
};
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

/// One settled interaction (Wave B item 10): canonical children hold the
/// truth, accessors derive the rest. The old shape carried the same facts
/// four ways (`action` string + `canonical`, `settled` bool + `settle_reason`,
/// `after` frame + `capture`) which could silently disagree; now:
///
/// - the action lives in [`Self::action`] (`ActionEnvelope`-level:
///   `CanonicalAction` + visibility);
/// - settlement lives in [`Self::settle`] only — `settled()` and the reason
///   derive from it;
/// - frames live in [`Self::before_frame`] / [`Self::after_frame`] as
///   [`CanonicalFrame`]s (citable `frame:N` identity), the raw
///   [`ScreenState`] reachable through `.state`;
/// - the capture outcome is the single record of *why* the after-frame is
///   authoritative (matching-frame capture), never optional for a settled
///   act.
#[derive(Debug, Clone)]
pub struct InteractionTransaction {
    /// The action envelope: canonical action + visibility policy.
    pub action: ActionEnvelope,
    /// The anchor that scoped the settle wait (captured before send).
    pub anchor: ObservationAnchor,
    /// The frame immediately before the input.
    pub before_frame: CanonicalFrame,
    /// The frame that satisfied the settle wait — the authoritative after
    /// (re-review P0 "matching frame"). For `SettleStatus::Skipped` this is
    /// a fresh post-action capture, clearly not a settled one.
    pub after_frame: CanonicalFrame,
    /// How the settle wait resolved.
    pub settle: SettleStatus,
    /// The before→after transition (screen + semantic diff).
    pub transition: Transition,
    /// The unified capture outcome for the after-frame (re-review P0).
    /// `None` only on the `no_wait` path, where no matching-frame capture
    /// happened.
    pub capture: Option<CaptureOutcome>,
    /// Total act latency (send + settle) in milliseconds.
    pub elapsed_ms: u64,
}

/// Action + persistence policy as one unit (leak fix): the executor always
/// receives the real action; every recorder consults `visibility`.
#[derive(Debug, Clone)]
pub struct ActionEnvelope {
    pub action: CanonicalAction,
    pub visibility: InputVisibility,
}

impl ActionEnvelope {
    pub fn new(action: CanonicalAction, visibility: InputVisibility) -> Self {
        ActionEnvelope { action, visibility }
    }

    /// The action's stable name.
    pub fn name(&self) -> &'static str {
        self.action.name()
    }
}

impl InteractionTransaction {
    /// The action name (derived — the old duplicated `action: String` field).
    pub fn name(&self) -> &'static str {
        self.action.name()
    }

    /// The typed canonical action, for replay/ledger.
    pub fn canonical(&self) -> &CanonicalAction {
        &self.action.action
    }

    /// Legacy read of settlement as a bool. `Skipped` counts as *not*
    /// settled — "we did not test" must not pass a settled assertion.
    pub fn settled(&self) -> bool {
        self.settle == SettleStatus::Met
    }

    /// Why the settle resolved (derived): `Some` reason string for every
    /// status, mirroring the old `settle_reason` field.
    pub fn settle_reason(&self) -> String {
        match self.settle {
            SettleStatus::Met => self
                .capture
                .as_ref()
                .map(|c| format!("{:?}", c.reason))
                .unwrap_or_else(|| "met".to_string()),
            SettleStatus::TimedOut => "settle_budget_exhausted".to_string(),
            SettleStatus::Skipped => "no_wait".to_string(),
        }
    }

    /// The screen state before the action.
    pub fn before(&self) -> &ScreenState {
        &self.before_frame.state
    }

    /// The screen state after the action (the matching frame when settled).
    pub fn after(&self) -> &ScreenState {
        &self.after_frame.state
    }

    /// Screen sequence of the after-frame, if the capture recorded one.
    pub fn after_screen_seq(&self) -> Option<u64> {
        self.capture.as_ref().map(|c| c.screen_seq)
    }
}

/// Execute one act against a session with proper causality:
///
/// 1. capture the event baseline *before* sending;
/// 2. send the input (routed through `send_unrecorded` when the visibility
///    policy is `Sensitive`/`NeverPersist`, so the cast never sees the
///    payload bytes — leak fix);
/// 3. wait for a screen stable *anchored after the baseline* (closes the
///    entry-pump race — re-review item 8);
/// 4. report the true [`SettleStatus`] — `Skipped` under `no_wait`, never a
///    fake "settled" (re-review item 9 / P1 fix 8).
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
    execute_act_with_visibility(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        InputVisibility::Normal,
    )
}

/// [`execute_act`] with an explicit [`InputVisibility`] policy. Sensitive
/// visibility routes the send through [`Session::send_unrecorded`] and marks
/// the transaction so every downstream recorder redacts.
pub fn execute_act_with_visibility(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
) -> Result<InteractionTransaction, anyhow::Error> {
    let before = match session.last().cloned() {
        Some(s) => s,
        None => session.observe(0)?,
    };
    let baseline = session.event_state();
    let anchor = ObservationAnchor {
        state: baseline,
        label: None,
        // Real monotonic per-session anchor index (re-review P1 fix 9).
        index: session.next_anchor(),
    };
    let mut before_frame = CanonicalFrame::new(before, 0, baseline.output_seq);
    before_frame.session_id = Some(session.id.clone());
    before_frame.generation = Some(session.generation);

    // Sensitive payloads bypass the recording hook for the WHOLE transaction
    // window (leak fix): the send is gated on input, and the settle wait's
    // captures are gated on output because the tty line discipline echoes
    // typed bytes back — the echo IS the payload. The PTY still receives
    // everything; only the cast is blind during the window.
    let sensitive_window = matches!(
        visibility,
        InputVisibility::Sensitive | InputVisibility::NeverPersist
    );
    if sensitive_window {
        session.suppress_recording();
    }
    let send_result = session.send(action.to_input());
    let send_failed = send_result.is_err();
    if send_failed {
        // Never leave the gates raised on an error path.
        if sensitive_window {
            session.resume_recording();
        }
        send_result?;
    }

    let (settle, elapsed_ms, after_state, after_screen_seq, capture) = if no_wait {
        // Even with no_wait, capture a fresh frame so callers always get
        // both before and after — but settlement was NOT tested. Reporting
        // `SettleStatus::Skipped` is the honest answer (re-review P1 fix 8).
        let s = session.observe(quiet_ms)?;
        (SettleStatus::Skipped, 0, s, None, None)
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
        let settle = if outcome.met {
            SettleStatus::Met
        } else {
            SettleStatus::TimedOut
        };
        let seq = outcome.screen_seq;
        (
            settle,
            outcome.elapsed_ms,
            outcome.state,
            Some(seq),
            Some(capture),
        )
    };

    let transition = crate::screen::diff(&before_frame.state, &after_state);
    let mut after_frame = CanonicalFrame::new(
        after_state,
        after_screen_seq.unwrap_or(0),
        capture.as_ref().map(|c| c.output_seq).unwrap_or(0),
    );
    after_frame.session_id = Some(session.id.clone());
    after_frame.generation = Some(session.generation);

    // Sensitive window closed: the settled frame has been captured, so the
    // application's own (masked) rendering is recorded again from here on.
    if sensitive_window {
        session.resume_recording();
    }

    Ok(InteractionTransaction {
        action: ActionEnvelope::new(action.clone(), visibility),
        anchor,
        before_frame,
        after_frame,
        settle,
        transition,
        capture,
        elapsed_ms,
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
        assert_eq!(tx.settle, SettleStatus::Met, "{}", tx.settle_reason());
        assert!(tx.settled(), "legacy bool must agree with the status");
        assert_eq!(tx.name(), "key");
        // echoed char must appear in the after frame
        assert!(
            tx.after().viewport_text.iter().any(|r| r.contains('x')),
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
        // no_wait must be Skipped, not settled (re-review P1 fix 8).
        assert_eq!(tx.settle, SettleStatus::Skipped);
        assert!(
            !tx.settled(),
            "Skipped must not read as settled through the legacy bool"
        );
        assert_eq!(tx.settle_reason().as_str(), "no_wait");
        assert_eq!(tx.elapsed_ms, 0);
    }

    /// Anchor indices are real per-session monotonic counters (P1 fix 9),
    /// not hardcoded zeros.
    #[test]
    fn anchor_indices_are_monotonic() {
        let mut s = session();
        let a = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('a')),
            },
            60,
            1000,
            true,
        )
        .expect("act 1");
        let b = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('b')),
            },
            60,
            1000,
            true,
        )
        .expect("act 2");
        let ia = a.anchor.index;
        let ib = b.anchor.index;
        assert!(ib > ia, "anchor index must increase: {} then {}", ia, ib);
    }

    /// A sensitive action's bytes must never reach the session's cast
    /// recording (leak fix, live half), while the PTY still receives them —
    /// the echo in the after-frame proves delivery.
    #[test]
    fn sensitive_send_bypasses_recording_but_reaches_pty() {
        let mut s = session();
        s.enable_recording(true);
        let secret = "hunter2-secret-payload";
        let tx = execute_act_with_visibility(
            &mut s,
            &CanonicalAction::Type {
                text: secret.to_string(),
            },
            80,
            1500,
            false,
            InputVisibility::Sensitive,
        )
        .expect("sensitive act");
        // PTY got the bytes: python echoes them back to the screen.
        assert!(
            tx.after()
                .viewport_text
                .iter()
                .any(|r: &String| r.contains(secret)),
            "payload must reach the PTY and be echoed"
        );
        // ...but not the recording.
        let cast = s
            .recorder()
            .expect("recorder")
            .lock()
            .expect("recorder lock")
            .to_ndjson()
            .join("\n");
        assert!(
            !cast.contains(secret),
            "sensitive payload must not leak into the recording"
        );
        // Unprotected sends still record (control).
        let _ = execute_act(
            &mut s,
            &CanonicalAction::Type {
                text: "visible-text".to_string(),
            },
            80,
            1500,
            false,
        )
        .expect("normal act");
        let cast2 = s
            .recorder()
            .expect("recorder")
            .lock()
            .expect("recorder lock")
            .to_ndjson()
            .join("\n");
        assert!(
            cast2.contains("visible-text"),
            "non-sensitive input must still be recorded"
        );
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
