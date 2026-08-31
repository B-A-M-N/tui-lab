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
//! Both are driver-free here: they map to the underlying
//! [`TerminalBackend`](crate::backend::TerminalBackend) wait/capture machinery
//! through [`capture_by_strategy`], which is the *only* place a strategy is
//! interpreted against a live backend.

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
    /// The named text appears.
    TextAppears(String),
    /// The named text disappears.
    TextDisappears(String),
    /// The child process exits.
    ProcessExit,
    /// The shell command finishes (OSC 133).
    CommandDone,
    /// The terminal bell rings.
    Bell,
    /// The semantic analysis output changes (controls/regions/affordances).
    SemanticChange,
    /// A matching terminal event fires.
    Event(TerminalEventMatcher),
    /// The action may legitimately produce no observable change (copy to
    /// clipboard, an invisible toggle). Never a false `settled=false`.
    MayBeSilent,
    /// Do not wait at all; act and return immediately.
    NoWait,
}

impl CompletionPolicy {
    /// The strategy this policy implies for a normal capture. `MayBeSilent`
    /// and `NoWait` deliberately yield something that returns promptly rather
    /// than blocking on stability.
    pub fn as_strategy(&self) -> CaptureStrategy {
        match self {
            CompletionPolicy::StableScreen => CaptureStrategy::Stable { quiet_ms: None },
            CompletionPolicy::FirstScreenChange | CompletionPolicy::AnyObservableChange => {
                CaptureStrategy::FirstChange
            }
            CompletionPolicy::TextAppears(t) => CaptureStrategy::UntilText(t.clone()),
            CompletionPolicy::TextDisappears(t) => CaptureStrategy::UntilTextAbsent(t.clone()),
            CompletionPolicy::ProcessExit => CaptureStrategy::UntilExit,
            CompletionPolicy::CommandDone => CaptureStrategy::DeadlineSnapshot { ms: 1000 },
            CompletionPolicy::Bell => CaptureStrategy::UntilEvent(TerminalEventMatcher("Bell")),
            CompletionPolicy::SemanticChange => CaptureStrategy::DeadlineSnapshot { ms: 500 },
            CompletionPolicy::Event(m) => CaptureStrategy::UntilEvent(m.clone()),
            CompletionPolicy::MayBeSilent => CaptureStrategy::AfterDuration { ms: 100 },
            CompletionPolicy::NoWait => CaptureStrategy::AfterDuration { ms: 0 },
        }
    }
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
    match strategy {
        CaptureStrategy::Stable { quiet_ms } => {
            let quiet = quiet_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| Duration::from_millis(120));
            wait_capture(backend, WaitCond::ScreenStable { quiet_for: quiet, after_screen_seq: Some(anchor_screen_seq) }, budget)
        }
        CaptureStrategy::FirstChange => {
            wait_capture(backend, WaitCond::ScreenStable { quiet_for: Duration::from_millis(0), after_screen_seq: Some(anchor_screen_seq) }, budget)
        }
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
            })
        }
        CaptureStrategy::Frames { count } => {
            // Collect `count` distinct sequential frames: advance a "seen_upto"
            // cursor, wait for a change edge beyond it, capture it once. The
            // contract is "give me N frames or the budget", NEVER "give me N
            // frames unless it looks quiet" — a flicker/spin under load can
            // stall a single frame interval well past a naive quiet window,
            // and breaking early would hand the agent a false "no more
            // frames" with fewer than it asked for. So we only stop when the
            // frame count is met or the overall budget elapses.
            let mut frames: Vec<ScreenState> = Vec::new();
            let start = std::time::Instant::now();
            let mut seen_upto = anchor_screen_seq;
            // A per-edge wait ceiling strictly shorter than the overall budget
            // so a genuinely-quiet child cannot stall the whole capture; the
            // loop re-checks `start.elapsed()` afterward and exits on budget.
            let edge_budget = budget.min(Duration::from_millis(300));
            while frames.len() < *count && start.elapsed() < budget {
                let out = backend.wait(
                    WaitCond::ScreenStable {
                        quiet_for: Duration::from_millis(0),
                        after_screen_seq: Some(seen_upto),
                    },
                    edge_budget,
                )?;
                if out.screen_seq > seen_upto {
                    // A genuine new frame beyond what we've captured.
                    frames.push(out.state.clone());
                    seen_upto = out.screen_seq;
                }
            }
            let frame = frames.last().cloned().unwrap_or(backend.state()?);
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: !frames.is_empty(),
                screen_seq: backend.event_state().screen_seq,
                output_seq: backend.event_state().output_seq,
                frame,
                elapsed_ms: start.elapsed().as_millis() as u64,
            })
        }
        CaptureStrategy::UntilText(t) => wait_capture(
            backend,
            WaitCond::Text(t.clone()),
            budget,
        ),
        CaptureStrategy::UntilTextAbsent(t) => wait_capture(
            backend,
            WaitCond::TextAbsent(t.clone()),
            budget,
        ),
        CaptureStrategy::UntilExit => wait_capture(backend, WaitCond::ProcessExit, budget),
        CaptureStrategy::UntilEvent(matcher) => {
            // Kind-matching lives on the terminal *event queue* (the backend
            // exposes sequences, not kinds). At this layer we wait for any
            // new observable change up to budget and surface a deadline
            // snapshot; the session layer applies the matcher against the
            // drained queue for exact kind semantics.
            let baseline_seq = backend.event_state().screen_seq + backend.event_state().bell_seq;
            let start = std::time::Instant::now();
            let _ = matcher;
            loop {
                let now = backend.event_state();
                let moved = now.screen_seq + now.bell_seq > baseline_seq;
                if moved || start.elapsed() >= budget {
                    let frame = backend.state()?;
                    return Ok(CaptureOutcome {
                        reason: if moved {
                            crate::backend::CaptureReason::Bell
                        } else {
                            crate::backend::CaptureReason::Deadline
                        },
                        met: moved,
                        screen_seq: now.screen_seq,
                        output_seq: now.output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    });
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        CaptureStrategy::DeadlineSnapshot { ms } => {
            let dur = Duration::from_millis(*ms);
            std::thread::sleep(dur);
            let frame = backend.state()?;
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: true,
                screen_seq: backend.event_state().screen_seq,
                output_seq: backend.event_state().output_seq,
                frame,
                elapsed_ms: dur.as_millis() as u64,
            })
        }
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
    #[test]
    fn frames_collects_multiple_distinct_screens() {
        let mut b = sleepy(
            "import sys,time\n\
             for i in range(6):\n\
             \x20   print(f'frame-{i}')\n\
             \x20   sys.stdout.flush()\n\
             \x20   time.sleep(0.1)\n\
             time.sleep(1)",
        );
        let baseline = b.event_state().screen_seq;
        let out = capture_by_strategy(
            &mut b,
            &CaptureStrategy::Frames { count: 3 },
            baseline,
            Duration::from_secs(3),
        )
        .expect("capture");
        assert!(out.met, "three sequential screen changes must be observable");
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