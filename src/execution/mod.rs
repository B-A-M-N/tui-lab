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
use crate::capture::CompletionPolicy;
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
/// RAII recording-window guard for a single act (Wave G review P1 18:
/// "mutations through transactions").
///
/// An act's mutation window may suppress recording (sensitive visibility) and
/// raise an observation anchor. Both must be released on *every* exit path —
/// success, error, early return, panic — or the next call observes a leaked
/// suppression and the session's recording stays blind. The guard owns that
/// obligation: it is constructed with the recording state it must restore and
/// its `Drop` guarantees restoration even when a later step (e.g. the settle
/// wait) returns `Err`.
struct ActTransactionGuard<'a> {
    session: &'a mut Session,
    /// Whether the constructor suppressed recording for this window.
    suppressed: bool,
}

impl<'a> ActTransactionGuard<'a> {
    fn begin(
        session: &'a mut Session,
        suppress: bool,
    ) -> ActTransactionGuard<'a> {
        if suppress {
            session.suppress_recording();
        }
        ActTransactionGuard { session, suppressed: suppress }
    }

    /// Reborrow the session for the transaction body. The reborrow lives only
    /// as long as the body's reads/writes; it is refreshed each call so `?`
    /// returns inside the body do not hold it past the guard's lifetime.
    fn sess(&mut self) -> &mut Session {
        &mut *self.session
    }

    /// Success path: restore recording, then relinquish the session.
    fn commit(mut self) {
        self.restore();
        // Swallow self so `Drop` doesn't double-restore.
        std::mem::forget(self);
    }

    /// Error path: restore recording so a later `?` returns a clean session.
    /// Idempotent with `Drop`, so callers may invoke it or not.
    fn restore(&mut self) {
        if self.suppressed {
            self.suppressed = false;
            self.session.resume_recording();
        }
    }
}

impl<'a> Drop for ActTransactionGuard<'a> {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn execute_act(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
) -> Result<InteractionTransaction, anyhow::Error> {
    execute_act_with_completion(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        InputVisibility::Normal,
        CompletionPolicy::StableScreen,
    )
}

/// [`execute_act`] with an explicit [`InputVisibility`] policy. Sensitive
/// visibility routes the send through [`Session::send_unrecorded`] and marks
/// the transaction so every downstream recorder redacts. The completion
/// policy is the ordinary "action ⇒ stable screen" case.
pub fn execute_act_with_visibility(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
) -> Result<InteractionTransaction, anyhow::Error> {
    execute_act_with_completion(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        visibility,
        CompletionPolicy::StableScreen,
    )
}

/// The canonical act executor with a declared [`CompletionPolicy`] (review
/// P0: "the runtime still assumes action success usually means a stable
/// screen change"). Every action that is *not* "send then screen settles" —
/// copy-to-clipboard (silent), quit (process exit), "type until text
/// appears", a bell-only notify — historically produced a false
/// `settled=false` because the settle wait demanded screen quiet that never
/// came. A `completion` policy tells the executor what "done" actually means
/// for THIS action, so a silent/toggle/exit action is correctly reported as
/// `Met` rather than a spurious timeout.
pub fn execute_act_with_completion(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
    completion: CompletionPolicy,
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
    // The ONE compiler runs on the pre-action frame before it moves into
    // the transaction evidence (re-review P0).
    let plan = crate::capture::compile_completion(&completion, &baseline, &before);
    let mut before_frame = CanonicalFrame::new(before, 0, baseline.output_seq);
    before_frame.session_id = Some(session.id.clone());
    before_frame.generation = Some(session.generation);

    // Sensitive payloads bypass the recording hook for the WHOLE transaction
    // window (leak fix): the send is gated on input, and the settle wait's
    // captures are gated on output because the tty line discipline echoes
    // typed bytes back — the echo IS the payload. The PTY still receives
    // everything; only the cast is blind during the window.
    //
    // `ActTransactionGuard` owns the window as the transaction boundary: the
    // send and its settle wait run inside it, and whichever way the function
    // exits — a successful `commit`, a send error, or a settle-wait `Err` —
    // the guard restores recording. A failed mutation never leaves the session
    // permanently blind (Wave G review P1 18).
    let sensitive_window = matches!(
        visibility,
        InputVisibility::Sensitive | InputVisibility::NeverPersist
    );
    let mut window = ActTransactionGuard::begin(session, sensitive_window);
    let send_result = window.sess().send(action.to_input());
    let send_failed = send_result.is_err();
    if send_failed {
        send_result?;
    }

    // A declared `NoWait` completion is equivalent to the transport's
    // no_wait flag: act, capture a fresh frame, report settlement as Skipped
    // (never a fake "settled").
    let skip = no_wait || matches!(completion, CompletionPolicy::NoWait);
    let (settle, elapsed_ms, after_state, after_screen_seq, capture) = if skip {
        // Even with no_wait, capture a fresh frame so callers always get
        // both before and after — but settlement was NOT tested. Reporting
        // `SettleStatus::Skipped` is the honest answer (re-review P1 fix 8).
        let s = window.sess().observe(quiet_ms)?;
        (SettleStatus::Skipped, 0, s, None, None)
    } else {
        let budget = settle_budget_ms.max(quiet_ms.saturating_add(1000));
        // The ONE compiler (re-review P0): every policy becomes a
        // CompletionPlan here; there is no second interpretation anywhere.
        let outcome = run_completion_plan(window.sess(), plan, quiet_ms, budget)?;
        let capture = outcome;
        // `MayBeSilent` is special: the action may legitimately produce no
        // observable change (clipboard copy, an invisible toggle). Silence
        // here is a Met by construction — the plan reports it, never a
        // false `settled=false`.
        let settle = if capture.met {
            SettleStatus::Met
        } else {
            SettleStatus::TimedOut
        };
        let seq = capture.screen_seq;
        (
            settle,
            capture.elapsed_ms,
            capture.frame.clone(),
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
    after_frame.session_id = Some(window.sess().id.clone());
    after_frame.generation = Some(window.sess().generation);

    // Sensitive window closed: the settled frame has been captured, so the
    // application's own (masked) rendering is recorded from here on. `commit`
    // restores recording and relinquishes the session.
    window.commit();

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

/// The ONE evaluator for compiled completion plans (re-review P0: exactly
/// one interpreter). `BackendWait`/`Bell` conditions run against the
/// backend's event-sequenced wait, anchored to the pre-action baseline.
/// Everything the backend cannot prove — real semantic change, arbitrary
/// event kinds, causally-anchored text transitions — is evaluated HERE,
/// above the backend layer, where the session's event queue and semantic
/// analysis live.
fn run_completion_plan(
    session: &mut Session,
    plan: crate::capture::CompletionPlan,
    quiet_ms: u64,
    budget_ms: u64,
) -> anyhow::Result<CaptureOutcome> {
    use crate::capture::CompletionPlan as Plan;
    let budget = std::time::Duration::from_millis(budget_ms);
    let baseline = session.event_state();
    match plan {
        // Backend-proven conditions: one `wait_after` call each.
        Plan::BackendWait(cond) => {
            Ok(CaptureOutcome::from_wait(
                session.wait_after(baseline, cond, budget_ms)?,
            ))
        }
        Plan::Bell(after_bell_seq) => Ok(CaptureOutcome::from_wait(session.wait_after(
            baseline,
            WaitCond::Bell {
                // The compile step already anchored this to the pre-action
                // bell counter; `wait_after` must not overwrite it.
                after_bell_seq: Some(after_bell_seq),
            },
            budget_ms,
        )?)),
        Plan::ProcessExit => Ok(CaptureOutcome::from_wait(
            session.wait_after(baseline, WaitCond::ProcessExit, budget_ms)?,
        )),
        Plan::CommandDone => Ok(CaptureOutcome::from_wait(session.wait_after(
            baseline,
            WaitCond::CommandDone {
                after_command_seq: None,
            },
            budget_ms,
        )?)),
        // Any observable edge — screen, bell, title, cursor — beyond the
        // action anchor. Genuinely broader than a screen change (re-review
        // P0: bell-only / title-only reactions count).
        Plan::AnyActivity => Ok(CaptureOutcome::from_wait(session.wait_after(
            baseline,
            WaitCond::AnyActivity {
                after_interaction_seq: Some(baseline.interaction_seq),
            },
            budget_ms,
        )?)),
        // The action may be silent: a SHORT grace window (not the whole
        // settle budget). A change inside the window is captured; silence
        // ends the wait as a successful completion with reason
        // `Idle`-proxied NoChange semantics (re-review P0 efficiency).
        Plan::SilentGrace(grace) => {
            let cond = WaitCond::AnyActivity {
                after_interaction_seq: Some(baseline.interaction_seq),
            };
            let outcome = session.wait_after(baseline, cond, grace.as_millis() as u64)?;
            let mut out = CaptureOutcome::from_wait(outcome.clone());
            if !outcome.met {
                // No observable change in the grace window: the honest
                // reading of a silent action. Re-frame as success.
                out.frame = session.observe(quiet_ms)?;
                out.screen_seq = session.event_state().screen_seq;
                out.output_seq = session.event_state().output_seq;
                out.reason = crate::backend::CaptureReason::Idle;
                out.met = true;
            }
            Ok(out)
        }
        // Everything below is evaluated by polling the session's own
        // state — the backend has no primitive for these predicates.
        Plan::Event(matcher, _compiled_anchor) => {
            let start = std::time::Instant::now();
            let anchor_seq = session.event_queue_stats()
                .get("last_seq")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            loop {
                let batch = session.events_since(anchor_seq);
                for ev in &batch.events {
                    if matcher.matches(&ev.kind) {
                        let frame = session.observe(quiet_ms)?;
                        return Ok(CaptureOutcome {
                            reason: crate::backend::CaptureReason::ScreenChanged,
                            met: true,
                            screen_seq: session.event_state().screen_seq,
                            output_seq: session.event_state().output_seq,
                            frame,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            frames: None,
                        });
                    }
                }
                if start.elapsed() >= budget {
                    let frame = session.observe(quiet_ms)?;
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        // REAL semantic change (re-review P0): wait for any frame edge, then
        // compare semantic identities. A spinner/clock/style-only change
        // keeps identity equal and the wait continues; a genuine control /
        // region / focus change resolves. Also resolves when the native
        // side-channel (if any) shifts the fused semantics.
        Plan::SemanticChange(before_identity) => {
            let start = std::time::Instant::now();
            let anchor_screen = baseline.screen_seq;
            loop {
                let now = session.event_state();
                if now.screen_seq > anchor_screen {
                    let frame = session.observe(quiet_ms)?;
                    if frame.semantic_identity() != before_identity {
                        return Ok(CaptureOutcome {
                            reason: crate::backend::CaptureReason::ScreenChanged,
                            met: true,
                            screen_seq: session.event_state().screen_seq,
                            output_seq: session.event_state().output_seq,
                            frame,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            frames: None,
                        });
                    }
                }
                if start.elapsed() >= budget {
                    let frame = session.observe(quiet_ms)?;
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        // Causally-anchored text transitions (re-review P0): `appears`
        // requires the text to be ABSENT at anchor and present after;
        // `disappears` the reverse. Presence at the anchor can never
        // satisfy an `appears` — the compiler recorded `was_present` and
        // the evaluator enforces the transition.
        Plan::TextAppears(text, was_present) => {
            let start = std::time::Instant::now();
            loop {
                let frame = session.observe(quiet_ms.min(30))?;
                let present = crate::capture::screen_contains(&frame, &text);
                if present && !was_present {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::TextMatched,
                        met: true,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                if start.elapsed() >= budget {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Plan::TextDisappears(text, was_present) => {
            let start = std::time::Instant::now();
            if !was_present {
                // Text was never there: the transition already holds. One
                // fresh frame documents it; no wait is needed.
                let frame = session.observe(quiet_ms)?;
                return Ok(CaptureOutcome {
                    reason: crate::backend::CaptureReason::TextAbsent,
                    met: true,
                    screen_seq: session.event_state().screen_seq,
                    output_seq: session.event_state().output_seq,
                    frame,
                    elapsed_ms: 0,
                    frames: None,
                });
            }
            loop {
                let frame = session.observe(quiet_ms.min(30))?;
                if !crate::capture::screen_contains(&frame, &text) {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::TextAbsent,
                        met: true,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                if start.elapsed() >= budget {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Plan::Immediate => {
            let frame = session.observe(quiet_ms)?;
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: true,
                screen_seq: session.event_state().screen_seq,
                output_seq: session.event_state().output_seq,
                frame,
                elapsed_ms: 0,
                frames: None,
            })
        }
    }
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

    /// The mutual-exclusion between: a send failure in a sensitive act must
    /// tear down the whole transaction — the guard restores recording even
    /// though no `commit` was reached (Wave G review P1 18, error path).
    #[test]
    fn failed_sensitive_act_restores_recording_via_guard() {
        let mut s = session();
        s.enable_recording(true);
        // Stop the child so the next `send` fails; sending to a dead PTY is
        // the deterministic analogue of any mid-transaction failure.
        s.stop().expect("stop child");

        let err = execute_act_with_visibility(
            &mut s,
            &CanonicalAction::Type {
                text: "should-not-matter".to_string(),
            },
            40,
            1000,
            false,
            InputVisibility::Sensitive,
        );
        assert!(err.is_err(), "a send to a stopped child must fail");

        // Recording must be restored on the error path — a leaked suppression
        // would leave the session permanently blind.
        assert!(
            s.is_recording(),
            "the transaction guard must restore recording on a failed send"
        );
    }

    // ── CompletionPolicy wiring (review P0 rigidity) ──────────────────

    /// A `ProcessExit` completion declares "the action's real success signal is
    /// the child exiting", NOT a screen settle. `execute_act` must wait for the
    /// exit and report `Met` — not a spurious stable-screen timeout.
    #[test]
    fn process_exit_completion_waits_for_exit_not_screen_settle() {
        let mut s = Session::new("exec-exit".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time; print('bye'); sys.stdout.flush(); time.sleep(0.2)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        // Wait for initial output so the process is visibly alive before the act.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let tx = execute_act_with_completion(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('q')),
            },
            40,
            2000,
            false,
            InputVisibility::Normal,
            CompletionPolicy::ProcessExit,
        )
        .expect("act with process-exit completion");
        // The child exits ~200ms after start; the completion proves the exit.
        assert_eq!(
            tx.settle,
            SettleStatus::Met,
            "process-exit completion must report Met, got {}",
            tx.settle_reason()
        );
        assert!(
            !tx.after().process.running,
            "after a process-exit completion the child must be gone"
        );
        s.stop().ok();
    }

    /// A `MayBeSilent` completion MUST never produce a false `settled=false`:
    /// the action may legitimately change nothing observable (clipboard copy,
    /// an invisible toggle). The executor captures whatever the screen shows
    /// and reports `Met` — the exact rigidity the review called out.
    #[test]
    fn may_be_silent_never_reports_false_timeout() {
        let mut s = Session::new("exec-silent".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import time; print('quiet'); time.sleep(3)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(300));

        let tx = execute_act_with_completion(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
            },
            40,
            300,
            false,
            InputVisibility::Normal,
            CompletionPolicy::MayBeSilent,
        )
        .expect("act");
        // Even if the typed char echoes, no *settle* was required; and if the
        // screen stayed quiet the action is still a success. Either way Met.
        assert_eq!(
            tx.settle,
            SettleStatus::Met,
            "MayBeSilent must report Met, never a false timeout"
        );
        s.stop().ok();
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
            completion: None,
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
            completion: None,
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
            completion: None,
            wait_ms: None,
            id: None,
        };
        assert!(
            CanonicalAction::from_request(&raw).is_err(),
            "empty raw rejected"
        );
    }
}
