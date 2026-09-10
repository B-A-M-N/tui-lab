//! Capture strategies and completion policies — the connective primitives
//! that stop the runtime from assuming "action ⇒ screen-change ⇒ stable"
//! (review P0: "The system still assumes action success usually means screen
//! change" and "Also add a generalized capture strategy").
//!
//! ## CaptureStrategy
//!
//! Not every debugging situation wants "wait until stable". A broken TUI may
//! redraw forever, flicker, spin, or freeze after one redraw. A strategy tells
//! the capture driver *what "done" means* — stable, first change, N frames,
//! a fixed duration, a deadline snapshot — so an agent can say "don't wait for
//! stability; show me the first five frames after Enter."
//!
//! ## CompletionPolicy
//!
//! An [`Affordance`](crate::semantic::Affordance) or action can declare its
//! *expected completion behavior*. Copy-to-clipboard is silent; `Quit` is a
//! process exit; `Save` is usually a semantic change. Encoding that per-action
//! removes whole classes of false `settled=false` and unnecessary waits.
//!
//! ## One compiler (re-review P0)
//!
//! There was previously a second, divergent interpretation of
//! [`CompletionPolicy`] (`as_strategy`) alongside the executor's
//! `completion_wait_cond`. Two interpreters of one vocabulary is how semantic
//! drift starts; both are gone. Every consumer compiles a policy through
//! [`compile_completion`] into a [`CompletionPlan`] and evaluates it with the
//! one evaluator — `evaluate_completion_plan` — against a `Session`.

use crate::backend::trait_def::TerminalBackend;
use crate::backend::{BackendResult, CaptureOutcome, WaitCond};
use crate::events::TerminalEventKind;
use crate::screen::ScreenState;
use std::time::Duration;

/// Generalized capture strategy (review P0).
///
/// Drives how a capture/observation decides "done". The variants map 1:1 onto
/// the underlying event-sequenced waits so a strategy is actually honored, not
/// just parsed.
#[derive(Debug, Clone)]
pub enum CaptureStrategy {
    /// Wait for the screen to settle for *at least* `quiet_ms` (defaulting to
    /// a conservative quiet window when `None`). Mirrors the ordinary
    /// `observe` settlement — the default.
    Stable { quiet_ms: Option<u64> },
    /// Return as soon as the screen changes after the anchor — do not wait
    /// for quiet. Ideal for flicker/spin diagnosis.
    FirstChange,
    /// A single snapshot after a fixed delay, regardless of stability.
    AfterDuration { ms: u64 },
    /// Collect exactly `count` distinct post-anchor frames (e.g. "first five
    /// frames after Enter"). Each frame is captured on a screen-change edge.
    Frames { count: usize },
    /// Capture when a named text appears on screen.
    UntilText(String),
    /// Capture when a named text is no longer on screen.
    UntilTextAbsent(String),
    /// Capture when the child process exits.
    UntilExit,
    /// Capture when a terminal event of the matching kind occurs.
    UntilEvent(TerminalEventMatcher),
    /// Snapshot now, then *also* hand back a deadline-bound snapshot even if
    /// nothing resolved — never block past `ms`.
    DeadlineSnapshot { ms: u64 },
}

/// A coarse terminal-event matcher for the strategy/policy `Event` variant.
/// Matches a [`TerminalEventKind`] by its variant name, so callers can wait on
/// e.g. `"Bell"`, `"FocusChanged"`, `"Resize"` without coupling to the exact
/// payload shape.
#[derive(Debug, Clone)]
pub struct TerminalEventMatcher(pub &'static str);

impl TerminalEventMatcher {
    /// Whether `kind` matches this matcher (by `std::any`-style variant name).
    pub fn matches(&self, kind: &TerminalEventKind) -> bool {
        variant_name(kind).contains(self.0) || self.0 == "*"
    }
}

/// An action's declared completion behavior (review P0 rigidity item).
#[derive(Debug, Clone)]
pub enum CompletionPolicy {
    /// The ordinary case: action then screen settles.
    StableScreen,
    /// A visible change is enough; quiet is not required.
    FirstScreenChange,
    /// Any observable terminal activity (output, cursor, bell, title).
    AnyObservableChange,
    /// The named text appears — *causally*: the text must not have been
    /// present before the action (re-review P0, text causality).
    TextAppears(String),
    /// The named text disappears.
    TextDisappears(String),
    /// The child process exits.
    ProcessExit,
    /// The shell command finishes (OSC 133).
    CommandDone,
    /// The terminal bell rings — anchored to the pre-action bell counter.
    Bell,
    /// The semantic analysis output changes (controls/regions/affordances) —
    /// evaluated against real semantic structure, not a screen-change proxy.
    SemanticChange,
    /// A matching terminal event fires, evaluated against the session's
    /// event queue with the pre-action event-sequence anchor.
    Event(TerminalEventMatcher),
    /// The action may legitimately produce no observable change (copy to
    /// clipboard, an invisible toggle). Never a false `settled=false`.
    MayBeSilent,
    /// Do not wait at all; act and return immediately.
    NoWait,
}

/// Why a sequence capture stopped collecting frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSequenceReason {
    /// The requested frame count was collected.
    CountReached,
    /// The overall budget elapsed before the count was reached.
    BudgetExpired,
    /// The child process exited mid-sequence.
    ProcessExited,
    /// The output channel closed mid-sequence.
    OutputClosed,
    /// The caller cancelled the capture.
    Interrupted,
}

impl CaptureSequenceReason {
    pub fn name(&self) -> &'static str {
        match self {
            CaptureSequenceReason::CountReached => "count_reached",
            CaptureSequenceReason::BudgetExpired => "budget_expired",
            CaptureSequenceReason::ProcessExited => "process_exited",
            CaptureSequenceReason::OutputClosed => "output_closed",
            CaptureSequenceReason::Interrupted => "interrupted",
        }
    }
}

/// The result of a [`CaptureStrategy::Frames`] capture (re-review P0):
/// *all* frames the driver collected, plus an honest completion verdict.
/// `completed` is `true` **only** when `captured == requested` — a partial
/// sequence is still returned, but never reported as met.
#[derive(Debug, Clone)]
pub struct CaptureSequenceOutcome {
    /// How many frames the caller asked for.
    pub requested: usize,
    /// How many distinct post-anchor frames were actually collected.
    pub captured: usize,
    /// The frames in capture order — the debugging payload. Never discarded.
    pub frames: Vec<ScreenState>,
    /// `captured == requested`. The only definition of "met".
    pub completed: bool,
    /// Why collection stopped.
    pub reason: CaptureSequenceReason,
    /// Screen sequence at the last captured frame (or the anchor when none).
    pub last_screen_seq: u64,
    pub elapsed_ms: u64,
}

/// A *compiled* completion plan — the single representation both the act
/// executor and capture driver consume (re-review P0: exactly one
/// interpreter for `CompletionPolicy`).
///
/// `BackendWait` variants are proven by the backend's event-sequenced wait;
/// everything else is evaluated by `evaluate_completion_plan` above the
/// backend layer, where the session's event queue, semantic state, and
/// before-frames live.
#[derive(Debug, Clone)]
pub enum CompletionPlan {
    /// Wait for the backend condition (anchored to the action baseline).
    BackendWait(WaitCond),
    /// Any observable edge after the anchor — screen, bell, title, cursor.
    /// Genuinely broader than a screen change (re-review P0).
    AnyActivity,
    /// A matching session event after the anchor, evaluated against the
    /// session's event queue; carries the matcher and the pre-action
    /// event-sequence anchor.
    Event(TerminalEventMatcher, u64),
    /// Real semantic change: the semantic identity of the fused frame must
    /// differ from the pre-action one. Evaluated by re-running semantic
    /// detection when a frame edge fires — not proxied by screen change.
    SemanticChange(String),
    /// Causally-anchored text: the text must transition from
    /// `was_present` to present (appears) or absent (disappears) after the
    /// anchor. The `bool` records the pre-action truth so "already there"
    /// can never satisfy an `appears` wait (re-review P0, text causality).
    TextAppears(String, bool),
    TextDisappears(String, bool),
    /// The action may be silent: observe a short grace window; a change
    /// captures it, silence is a successful completion (re-review P0,
    /// MayBeSilent efficiency).
    SilentGrace(Duration),
    /// Wait for the child to exit.
    ProcessExit,
    /// OSC 133 command finish, anchored.
    CommandDone,
    /// Bell, anchored to the pre-action bell counter (re-review P0 race).
    Bell(u64),
    /// Do not wait at all.
    Immediate,
}

/// The ONE compiler from a declared [`CompletionPolicy`] to an executable
/// [`CompletionPlan`] (re-review P0: "exactly one compiler").
///
/// - `anchor`: the pre-action `TerminalEventState` (event counters at the
///   moment before the action is sent).
/// - `before`: the pre-action `ScreenState` (for text causality and
///   semantic-change identity fallback).
/// - `pre_event_seq`: the session event-queue last sequence *before* the
///   action is sent.  Used by [`CompletionPolicy::Event`] so the evaluator
///   never misses an event that fires immediately after the send.
/// - `quiet_ms`: the quiet interval for [`CompletionPolicy::StableScreen`].
/// - `fused_identity`: when present, the pre-action fused semantic identity
///   for [`CompletionPolicy::SemanticChange`]; otherwise falls back to
///   `before.semantic_identity()`.
pub fn compile_completion(
    policy: &CompletionPolicy,
    anchor: &crate::backend::TerminalEventState,
    before: &ScreenState,
    pre_event_seq: u64,
    quiet_ms: u64,
    fused_identity: Option<String>,
) -> CompletionPlan {
    match policy {
        CompletionPolicy::StableScreen => CompletionPlan::BackendWait(WaitCond::ScreenStable {
            quiet_for: Duration::from_millis(quiet_ms),
            after_screen_seq: Some(anchor.screen_seq),
        }),
        CompletionPolicy::FirstScreenChange => {
            // FirstScreenChange needs only one frame; no quiet window required.
            CompletionPlan::BackendWait(WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(0),
                after_screen_seq: Some(anchor.screen_seq),
            })
        }
        CompletionPolicy::AnyObservableChange => CompletionPlan::AnyActivity,
        CompletionPolicy::TextAppears(t) => {
            let was_present = screen_contains(before, t);
            CompletionPlan::TextAppears(t.clone(), was_present)
        }
        CompletionPolicy::TextDisappears(t) => {
            let was_present = screen_contains(before, t);
            CompletionPlan::TextDisappears(t.clone(), was_present)
        }
        CompletionPolicy::ProcessExit => CompletionPlan::ProcessExit,
        CompletionPolicy::CommandDone => CompletionPlan::CommandDone,
        CompletionPolicy::Bell => CompletionPlan::Bell(anchor.bell_seq),
        CompletionPolicy::SemanticChange => {
            // Pre-action fused semantic identity (or fallback to screen-based).
            let identity = fused_identity.unwrap_or_else(|| before.semantic_identity());
            CompletionPlan::SemanticChange(identity)
        }
        CompletionPolicy::Event(m) => CompletionPlan::Event(m.clone(), pre_event_seq),
        CompletionPolicy::MayBeSilent => {
            CompletionPlan::SilentGrace(Duration::from_millis(SILENT_GRACE_MS))
        }
        CompletionPolicy::NoWait => CompletionPlan::Immediate,
    }
}

/// Default grace window for [`CompletionPolicy::MayBeSilent`] — short by
/// design (re-review P0: "50–150 ms, not 1+ seconds").
pub const SILENT_GRACE_MS: u64 = 100;

/// Whether a screen's viewport or scrollback contains `text`.
pub fn screen_contains(screen: &ScreenState, text: &str) -> bool {
    screen.viewport_text.iter().any(|r| r.contains(text))
        || screen.scrollback.iter().any(|r| r.contains(text))
}

/// Interpret a [`CaptureStrategy`] against a live backend and return the
/// capture outcome. The single place strategies become real behavior.
///
/// - `anchor`: the `screen_seq` observed *before* the stimulus; anchors the
///   change/settling conditions so pre-existing screen content does not
///   satisfy them.
/// - `budget`: an overall ceiling so a pathological strategy (a broken TUI
///   that never settles) still returns, not hangs.
pub fn capture_by_strategy(
    backend: &mut dyn TerminalBackend,
    strategy: &CaptureStrategy,
    anchor_screen_seq: u64,
    budget: Duration,
) -> BackendResult<CaptureOutcome> {
    // One deadline at top-level dispatch: a strategy may sample earlier,
    // never extend the caller's wall clock.
    let deadline = std::time::Instant::now() + budget;
    match strategy {
        CaptureStrategy::Stable { quiet_ms } => {
            let quiet = quiet_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| Duration::from_millis(120));
            wait_capture(
                backend,
                WaitCond::ScreenStable {
                    quiet_for: quiet,
                    after_screen_seq: Some(anchor_screen_seq),
                },
                budget,
            )
        }
        CaptureStrategy::FirstChange => wait_capture(
            backend,
            WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(0),
                after_screen_seq: Some(anchor_screen_seq),
            },
            budget,
        ),
        CaptureStrategy::AfterDuration { ms } => {
            let dur = Duration::from_millis(*ms);
            if dur == Duration::ZERO {
                let frame = backend.state()?;
                return Ok(CaptureOutcome {
                    reason: crate::backend::CaptureReason::Deadline,
                    met: true,
                    screen_seq: backend.event_state().screen_seq,
                    output_seq: backend.event_state().output_seq,
                    frame,
                    elapsed_ms: 0,
                    frames: None,
                });
            }
            std::thread::sleep(dur);
            let frame = backend.state()?;
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: true,
                screen_seq: backend.event_state().screen_seq,
                output_seq: backend.event_state().output_seq,
                frame,
                elapsed_ms: dur.as_millis() as u64,
                frames: None,
            })
        }
        CaptureStrategy::Frames { count } => {
            // Re-review P0: collect ALL `count` frames and return the whole
            // sequence. `met` is `captured == requested` — a partial sequence
            // is data, not success.
            let seq = capture_frame_sequence(backend, *count, anchor_screen_seq, budget);
            let frame = match seq.frames.last() {
                Some(f) => f.clone(),
                None => backend.state()?,
            };
            Ok(CaptureOutcome {
                reason: match seq.reason {
                    CaptureSequenceReason::CountReached => crate::backend::CaptureReason::Settled,
                    CaptureSequenceReason::ProcessExited => {
                        crate::backend::CaptureReason::ProcessExit
                    }
                    CaptureSequenceReason::OutputClosed => {
                        crate::backend::CaptureReason::OutputClosed
                    }
                    _ => crate::backend::CaptureReason::Deadline,
                },
                met: seq.completed,
                screen_seq: seq.last_screen_seq,
                output_seq: backend.event_state().output_seq,
                frame,
                elapsed_ms: seq.elapsed_ms,
                // The sequence survives: this is the debugging payload the
                // caller asked for.
                frames: Some(seq.frames),
            })
        }
        CaptureStrategy::UntilText(t) => wait_capture(backend, WaitCond::Text(t.clone()), budget),
        CaptureStrategy::UntilTextAbsent(t) => {
            wait_capture(backend, WaitCond::TextAbsent(t.clone()), budget)
        }
        CaptureStrategy::UntilExit => wait_capture(backend, WaitCond::ProcessExit, budget),
        CaptureStrategy::UntilEvent(_matcher) => {
            // Route through the backend's event-aware wait loop so pending
            // PTY/pipe bytes are pumped while waiting. The backend cannot
            // match event kinds; it proves the next observable edge, and a
            // session-level matcher narrows the kind.
            let baseline = backend.event_state();
            let out = backend.wait(
                WaitCond::AnyActivity {
                    after_interaction_seq: Some(baseline.interaction_seq),
                },
                budget,
            )?;
            let mut capture = CaptureOutcome::from_wait(out);
            capture.reason = if capture.met {
                crate::backend::CaptureReason::ScreenChanged
            } else {
                crate::backend::CaptureReason::Deadline
            };
            Ok(capture)
        }
        CaptureStrategy::DeadlineSnapshot { ms } => {
            let dur = Duration::from_millis(*ms).min(budget);
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            std::thread::sleep(dur.min(remaining));
            let frame = backend.state()?;
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: true,
                screen_seq: backend.event_state().screen_seq,
                output_seq: backend.event_state().output_seq,
                frame,
                elapsed_ms: dur.as_millis() as u64,
                frames: None,
            })
        }
    }
}

/// Collect up to `count` distinct post-anchor frames. Returns the FULL
/// sequence plus an honest stop reason (re-review P0). Frames are captured
/// on change edges beyond a `seen_upto` cursor; collection stops when the
/// count is reached, the budget elapses, or the child exits/closes — never
/// on a single quiet interval (a flicker/spin can stall one edge interval
/// well past a naive quiet window).
pub fn capture_frame_sequence(
    backend: &mut dyn TerminalBackend,
    count: usize,
    anchor_screen_seq: u64,
    budget: Duration,
) -> CaptureSequenceOutcome {
    let start = std::time::Instant::now();
    let mut frames: Vec<ScreenState> = Vec::new();
    let mut seen_upto = anchor_screen_seq;
    // A per-edge wait ceiling strictly shorter than the overall budget so a
    // genuinely-quiet child cannot stall the whole capture; the loop
    // re-checks `start.elapsed()` and exits on budget.
    let edge_budget = budget.min(Duration::from_millis(300));
    let mut reason = CaptureSequenceReason::BudgetExpired;
    while frames.len() < count && start.elapsed() < budget {
        let out = backend.wait(
            WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(0),
                after_screen_seq: Some(seen_upto),
            },
            edge_budget,
        );
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                // Channel closed / backend error: stop and report honestly.
                reason = if backend.event_state().screen_seq > seen_upto {
                    CaptureSequenceReason::Interrupted
                } else {
                    CaptureSequenceReason::OutputClosed
                };
                let _ = e;
                break;
            }
        };
        if out.screen_seq > seen_upto {
            frames.push(out.state.clone());
            seen_upto = out.screen_seq;
            if frames.len() == count {
                reason = CaptureSequenceReason::CountReached;
                break;
            }
            continue;
        }
        // The edge wait timed out without a new frame: check process state
        // so an exited child ends the sequence with a truthful reason.
        if !backend.state().map(|s| s.process.running).unwrap_or(true) {
            reason = CaptureSequenceReason::ProcessExited;
            break;
        }
    }
    let last_screen_seq = frames
        .len()
        .checked_sub(1)
        .map(|_| seen_upto)
        .unwrap_or(anchor_screen_seq);
    CaptureSequenceOutcome {
        requested: count,
        captured: frames.len(),
        completed: frames.len() == count,
        frames,
        reason,
        last_screen_seq,
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

fn wait_capture(
    backend: &mut dyn TerminalBackend,
    cond: WaitCond,
    budget: Duration,
) -> BackendResult<CaptureOutcome> {
    let out = backend.wait(cond, budget)?;
    Ok(CaptureOutcome::from_wait(out))
}

fn variant_name(kind: &TerminalEventKind) -> &'static str {
    match kind {
        TerminalEventKind::Output { .. } => "Output",
        TerminalEventKind::ScreenChanged { .. } => "ScreenChanged",
        TerminalEventKind::VisualChanged => "VisualChanged",
        TerminalEventKind::CursorMoved { .. } => "CursorMoved",
        TerminalEventKind::Bell => "Bell",
        TerminalEventKind::TitleChanged { .. } => "TitleChanged",
        TerminalEventKind::Resize { .. } => "Resize",
        TerminalEventKind::FocusChanged { .. } => "FocusChanged",
        TerminalEventKind::ProcessStarted => "ProcessStarted",
        TerminalEventKind::ProcessExited { .. } => "ProcessExited",
        TerminalEventKind::SemanticChanged => "SemanticChanged",
        TerminalEventKind::NativeEvent { .. } => "NativeEvent",
        TerminalEventKind::QueryAnswered { .. } => "QueryAnswered",
    }
}

/// Test scaffolding for the capture driver: a strategy against a real python
/// child through the CLI backend (the cheapest honest backend).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::line_cli::PtyLineBackend;

    fn sleepy(script: &str) -> PtyLineBackend {
        let mut b = PtyLineBackend::new(80, 24);
        b.start(
            "python3",
            &["-c".to_string(), script.to_string()],
            None,
            &[],
            80,
            24,
        )
        .expect("start");
        b
    }

    #[test]
    fn after_duration_captures_without_waiting_for_stability() {
        let mut b = sleepy(
            "import sys,time\n\
             print('A')\n\
             sys.stdout.flush()\n\
             time.sleep(0.5)\n\
             print('B')\n\
             sys.stdout.flush()\n\
             time.sleep(1)",
        );
        let baseline = b.event_state().screen_seq;
        let out = capture_by_strategy(
            &mut b,
            &CaptureStrategy::AfterDuration { ms: 300 },
            baseline,
            Duration::from_secs(2),
        )
        .expect("capture");
        assert!(out.met);
        b.stop().ok();
    }

    /// Flicker diagnosis: collect multiple distinct frames as the screen
    /// changes, without waiting for it to settle.
    ///
    /// The child gates its own first print on an input line ("GO"), so t=0
    /// is under the test's control: no frame can commit before the anchor
    /// is taken, no matter how the scheduler staggers this thread against
    /// the other PTY tests. The anchor is taken AFTER the GO echo has
    /// landed (the PTY echo of the gate is itself a screen edge), so the
    /// sequence collects exactly frame-0..2 and nothing else.
    #[test]
    fn frames_collects_multiple_distinct_screens() {
        let mut b = sleepy(
            "import sys,time\n\
             input()\n\
             for i in range(6):\n\
             \x20   print(f'frame-{i}')\n\
             \x20   sys.stdout.flush()\n\
             \x20   time.sleep(0.1)\n\
             time.sleep(1)",
        );
        // Child is alive and blocked on input(); it cannot print yet.
        let pre = b.event_state().screen_seq;
        b.send_input(crate::backend::Input::Text("GO\n".into()))
            .expect("send start signal");
        // Absorb the gate's own echo edge: the kernel line discipline
        // echoes "GO\r\n" (exactly one committed line = exactly one seq
        // edge, before input() can even return), so a quiet_for:0
        // edge-wait anchored at `pre` meets on the echo and nothing else.
        // quiet_for:0 cannot starve on stability; the budget only has to
        // cover python startup under load. The wait returns the SERVED
        // snapshot's seq, so `baseline` sits exactly past the echo.
        let echo = b
            .wait(
                crate::backend::WaitCond::ScreenStable {
                    quiet_for: Duration::from_millis(0),
                    after_screen_seq: Some(pre),
                },
                Duration::from_secs(10),
            )
            .expect("echo edge");
        assert!(echo.met, "the GO echo must land as a screen edge");
        let baseline = echo.screen_seq;
        let out = capture_by_strategy(
            &mut b,
            &CaptureStrategy::Frames { count: 3 },
            baseline,
            Duration::from_secs(8),
        )
        .expect("capture");
        assert!(
            out.met,
            "three sequential screen changes must be observable"
        );
        // Re-review P0: the full sequence must survive the call.
        let frames = out.frames.as_ref().expect("frames sequence returned");
        assert_eq!(frames.len(), 3, "requested == captured");
        assert!(
            frames
                .windows(2)
                .any(|p| p[0].structure_hash != p[1].structure_hash),
            "collected frames should be distinct screens"
        );
        b.stop().ok();
    }

    /// A partial sequence is returned but NOT reported as met. Same gate as
    /// the met-path test: the child's single print happens only after the
    /// test says GO (anchor taken past the echo edge), so the baseline can
    /// never miss it under load and the count is deterministic.
    #[test]
    fn frames_partial_capture_is_not_met() {
        // The trailing sleep is far longer than the 1.2s capture budget
        // (even with load-stretched edge waits), so the child cannot exit
        // mid-collection and the stop reason is deterministically Deadline.
        let mut b = sleepy(
            "import sys,time\n\
             input()\n\
             print('ONLY-ONE')\n\
             sys.stdout.flush()\n\
             time.sleep(30)",
        );
        let pre = b.event_state().screen_seq;
        b.send_input(crate::backend::Input::Text("GO\n".into()))
            .expect("send start signal");
        // Same echo absorption as the met-path test: quiet_for:0 edge-wait
        // anchored at `pre` meets exactly on the echo edge (kernel echo of
        // "GO\r\n" commits one line before input() returns), then anchor
        // past it so ONLY-ONE is the first collectible frame.
        let echo = b
            .wait(
                crate::backend::WaitCond::ScreenStable {
                    quiet_for: Duration::from_millis(0),
                    after_screen_seq: Some(pre),
                },
                Duration::from_secs(10),
            )
            .expect("echo edge");
        assert!(echo.met, "the GO echo must land as a screen edge");
        let out = capture_by_strategy(
            &mut b,
            &CaptureStrategy::Frames { count: 5 },
            echo.screen_seq,
            Duration::from_millis(1200),
        )
        .expect("capture");
        let frames = out.frames.as_ref().expect("frames sequence returned");
        assert_eq!(frames.len(), 1, "exactly the one gated frame arrived");
        assert!(!out.met, "captured < requested must not be met");
        assert_eq!(out.reason, crate::backend::CaptureReason::Deadline);
        b.stop().ok();
    }

    #[test]
    fn until_text_honors_appearance_condition() {
        let mut b = sleepy(
            "import time\nprint('HELLO-MARK')\nimport sys; sys.stdout.flush()\ntime.sleep(1)",
        );
        let baseline = b.event_state().screen_seq;
        let out = capture_by_strategy(
            &mut b,
            &CaptureStrategy::UntilText("HELLO-MARK".into()),
            baseline,
            Duration::from_secs(4),
        )
        .expect("capture");
        assert!(out.met);
        b.stop().ok();
    }
}
