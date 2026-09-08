//! Wait-condition semantics for the portable PTY backend (god-object
//! round 2, G2).
//!
//! The giant `match &cond` inside the backend's polling `wait()` loop
//! moves here as `WaitEvaluator`: given one condition, one snapshot of
//! what the loop observed this tick, and the baselines captured at wait
//! entry, decide whether the condition is met and which reason to
//! report. The polling loop itself (pump → sync counters → snapshot →
//! evaluate → sleep) stays on the backend — one canonical pipeline,
//! evaluation extracted.
//!
//! Pure decision policy: no PTY, no parser, no sleeps. Every condition
//! is unit-testable without a child process.

use crate::backend::{WaitCond, WaitReason};
use crate::screen::ScreenState;

/// What one wait-loop tick observed — the inputs evaluation reads.
pub(super) struct WaitTick<'a> {
    /// The materialized screen snapshot for this tick (viewport +
    /// scrollback + process + title).
    pub screen: &'a ScreenState,
    /// Current event-clock counters.
    pub screen_seq: u64,
    pub output_seq: u64,
    pub bell_seq: u64,
    pub interaction_seq: u64,
    /// Whether the screen has been quiet for the condition's interval
    /// (monotonic, pre-computed by the caller).
    pub screen_quiet: bool,
    /// Whether PTY output has been quiet for the condition's interval.
    pub output_quiet: bool,
    /// OSC 133 shell-integration counters from the parser callbacks.
    pub command_seq: u64,
    pub command_running: bool,
}

/// Baselines captured at wait entry.
#[derive(Clone)]
pub(super) struct WaitBaselines {
    /// Review P0 (Bell race): an anchored Bell carries its own baseline
    /// (`after_bell_seq - 1`); only an unanchored Bell falls back to
    /// "captured at wait entry".
    pub bell_seq: u64,
    /// Review P0 (AnyObservableChange): interaction counter at entry.
    pub interaction_seq: u64,
    /// For ScreenChange: the baseline *interaction fingerprint* captured
    /// after the entry pump (style-only changes count; audit item 3).
    pub fingerprint: String,
}

/// Evaluate one wait condition against one tick's observations.
pub(super) struct WaitEvaluator;

impl WaitEvaluator {
    pub(super) fn evaluate(
        cond: &WaitCond,
        tick: &WaitTick,
        baselines: &WaitBaselines,
        current_fingerprint: &str,
    ) -> (bool, WaitReason) {
        match cond {
            WaitCond::Text(t) => (
                tick.screen
                    .viewport_text
                    .iter()
                    .any(|r| r.contains(t.as_str())),
                WaitReason::Text,
            ),
            WaitCond::TextAbsent(t) => (
                !tick
                    .screen
                    .viewport_text
                    .iter()
                    .any(|r| r.contains(t.as_str())),
                WaitReason::TextAbsent,
            ),
            WaitCond::ScreenChange => (
                current_fingerprint != baselines.fingerprint,
                WaitReason::ScreenChange,
            ),
            WaitCond::ScreenStable {
                quiet_for: _,
                after_screen_seq,
            } => {
                // Generic stability: the screen has simply been quiet for
                // `quiet_for`. NO fresh mutation is required — a one-frame
                // reaction that arrived *before* wait() was entered must
                // still resolve (audit items 1/2/70).
                // Anchored stability: additionally require a screen change
                // with sequence strictly greater than the captured baseline
                // (wait_after / anchored_to).
                let anchored_ok = match after_screen_seq {
                    Some(seq) => tick.screen_seq > *seq,
                    None => true,
                };
                (anchored_ok && tick.screen_quiet, WaitReason::ScreenStable)
            }
            WaitCond::ProcessExit => (!tick.screen.process.running, WaitReason::ProcessExit),
            WaitCond::Title(t) => (
                tick.screen.title.as_deref() == Some(t.as_str()),
                WaitReason::Title,
            ),
            WaitCond::Bell { .. } => (tick.bell_seq > baselines.bell_seq, WaitReason::Bell),
            WaitCond::AnyActivity {
                after_interaction_seq,
            } => {
                // Any observable edge — screen, bell, title, cursor — with
                // an interaction sequence strictly greater than the
                // anchor. Genuinely broader than ScreenChange: a bell-only
                // or title-only reaction resolves here.
                let anchored_ok = match after_interaction_seq {
                    Some(seq) => tick.interaction_seq > *seq,
                    None => tick.interaction_seq > baselines.interaction_seq,
                };
                (anchored_ok, WaitReason::ScreenChange)
            }
            WaitCond::Idle {
                quiet_for: _,
                after_output_seq,
            } => {
                // Same shape as ScreenStable, keyed on PTY output chunks:
                // quiet interval suffices without an anchor; with an
                // anchor, require output strictly newer than the baseline.
                let anchored_ok = match after_output_seq {
                    Some(seq) => tick.output_seq > *seq,
                    None => true,
                };
                (anchored_ok && tick.output_quiet, WaitReason::Idle)
            }
            WaitCond::CommandDone { after_command_seq } => {
                // Wave F item 54: a finish edge (133;D) with sequence
                // strictly greater than the anchor resolved this wait.
                // `None` anchor semantics: the NEXT finish edge after
                // entering the wait — so the wait must have seen the
                // command both start and finish while it ran. With an
                // anchor, the finish simply has to be newer.
                let anchored_ok = match after_command_seq {
                    Some(seq) => tick.command_seq > *seq,
                    None => tick.command_seq > 0 && !tick.command_running,
                };
                let done_now = !tick.command_running;
                (anchored_ok && done_now, WaitReason::Idle)
            }
            WaitCond::CommandOutput {
                text,
                after_command_seq,
            } => {
                // Wave F item 54: the text must appear in output captured
                // for the anchored command — checked against the live
                // viewport only when the anchored command is the one
                // currently running or the last finished one, so a token
                // from an EARLIER command cannot satisfy this wait.
                let anchored_ok = match after_command_seq {
                    Some(seq) => tick.command_seq > *seq,
                    None => tick.command_seq > 0,
                };
                let in_window = tick.command_seq.saturating_sub(1)
                    == after_command_seq.unwrap_or(0)
                    || tick.command_seq == after_command_seq.unwrap_or(0).max(1);
                let found = anchored_ok
                    && in_window
                    && (tick
                        .screen
                        .viewport_text
                        .iter()
                        .any(|r| r.contains(text.as_str()))
                        || tick
                            .screen
                            .scrollback
                            .iter()
                            .any(|r| r.contains(text.as_str())));
                (found, WaitReason::Text)
            }
        }
    }
}

/// The backend consumes the evaluator through this shim so the signature
/// stays honest about what a tick carries (used by tests below).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::ProcessState;

    fn screen(text: &[&str]) -> ScreenState {
        let mut s = ScreenState::new(20, text.len() as u16);
        s.viewport_text = text.iter().map(|s| s.to_string()).collect();
        s.process = ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        };
        s
    }

    fn tick(screen: &ScreenState) -> WaitTick<'_> {
        WaitTick {
            screen,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            interaction_seq: 0,
            screen_quiet: false,
            output_quiet: false,
            command_seq: 0,
            command_running: false,
        }
    }

    fn baselines() -> WaitBaselines {
        WaitBaselines {
            bell_seq: 0,
            interaction_seq: 0,
            fingerprint: "fp0".into(),
        }
    }

    #[test]
    fn text_matches_viewport_rows() {
        let s = screen(&["hello", "world"]);
        let t = tick(&s);
        let (met, reason) =
            WaitEvaluator::evaluate(&WaitCond::Text("wor".into()), &t, &baselines(), "fp0");
        assert!(met);
        assert!(matches!(reason, WaitReason::Text));
    }

    #[test]
    fn text_absent_inverts() {
        let s = screen(&["hello"]);
        let t = tick(&s);
        let (met, _) =
            WaitEvaluator::evaluate(&WaitCond::TextAbsent("xyz".into()), &t, &baselines(), "fp0");
        assert!(met);
        let (met, _) =
            WaitEvaluator::evaluate(&WaitCond::TextAbsent("ell".into()), &t, &baselines(), "fp0");
        assert!(!met);
    }

    #[test]
    fn screen_change_compares_fingerprints() {
        let s = screen(&["a"]);
        let t = tick(&s);
        let (met, _) = WaitEvaluator::evaluate(&WaitCond::ScreenChange, &t, &baselines(), "fp0");
        assert!(!met, "same fingerprint does not resolve");
        let (met, _) = WaitEvaluator::evaluate(&WaitCond::ScreenChange, &t, &baselines(), "fp1");
        assert!(met, "a changed fingerprint resolves");
    }

    #[test]
    fn screen_stable_requires_quiet_and_anchor() {
        let s = screen(&["a"]);
        let mut t = tick(&s);
        let cond = WaitCond::ScreenStable {
            quiet_for: std::time::Duration::from_millis(50),
            after_screen_seq: None,
        };
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(!met, "not quiet yet");
        t.screen_quiet = true;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(met, "quiet without an anchor resolves (audit 1/2/70)");
        // Anchored: seq must strictly exceed the anchor even when quiet.
        let anchored = WaitCond::ScreenStable {
            quiet_for: std::time::Duration::from_millis(50),
            after_screen_seq: Some(3),
        };
        t.screen_seq = 3;
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(!met, "anchor equal to current seq does not resolve");
        t.screen_seq = 4;
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(met);
    }

    #[test]
    fn process_exit_reads_the_snapshot() {
        let mut s = screen(&["a"]);
        let t = tick(&s);
        let (met, _) = WaitEvaluator::evaluate(&WaitCond::ProcessExit, &t, &baselines(), "fp0");
        assert!(!met, "default process state is not running=false");
        s.process.running = false;
        let t = tick(&s);
        let (met, _) = WaitEvaluator::evaluate(&WaitCond::ProcessExit, &t, &baselines(), "fp0");
        assert!(met);
    }

    #[test]
    fn bell_is_strictly_greater_than_baseline() {
        let s = screen(&[]);
        let mut t = tick(&s);
        let cond = WaitCond::Bell {
            after_bell_seq: None,
        };
        t.bell_seq = 5;
        let mut b = baselines();
        b.bell_seq = 5;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &b, "fp0");
        assert!(!met, "equal counter is not a new bell");
        t.bell_seq = 6;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &b, "fp0");
        assert!(met);
    }

    #[test]
    fn any_activity_accepts_anchor_or_entry_baseline() {
        let s = screen(&[]);
        let mut t = tick(&s);
        t.interaction_seq = 9;
        let anchored = WaitCond::AnyActivity {
            after_interaction_seq: Some(10),
        };
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(!met);
        t.interaction_seq = 11;
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(met);
        // Unanchored: compare against the entry baseline — the tick's
        // counter must strictly exceed what it was at entry.
        let unanchored = WaitCond::AnyActivity {
            after_interaction_seq: None,
        };
        t.interaction_seq = 9;
        let mut b = baselines();
        b.interaction_seq = 9;
        let (met, _) = WaitEvaluator::evaluate(&unanchored, &t, &b, "fp0");
        assert!(!met, "counter equal to entry is not a new edge");
        t.interaction_seq = 10;
        let (met, _) = WaitEvaluator::evaluate(&unanchored, &t, &b, "fp0");
        assert!(met, "any advance past entry resolves");
    }

    #[test]
    fn idle_requires_quiet_and_anchor_like_screen_stable() {
        let s = screen(&[]);
        let mut t = tick(&s);
        let cond = WaitCond::Idle {
            quiet_for: std::time::Duration::from_millis(20),
            after_output_seq: None,
        };
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(!met);
        t.output_quiet = true;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(met);
    }

    #[test]
    fn command_done_needs_start_and_finish_when_unanchored() {
        let s = screen(&[]);
        let mut t = tick(&s);
        let cond = WaitCond::CommandDone {
            after_command_seq: None,
        };
        // A command that started and is still running: not done.
        t.command_seq = 1;
        t.command_running = true;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(!met);
        // Finished: done.
        t.command_running = false;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(met);
        // Anchored: the finish must be newer than the anchor even if
        // already finished.
        let anchored = WaitCond::CommandDone {
            after_command_seq: Some(2),
        };
        t.command_seq = 2;
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(!met);
        t.command_seq = 3;
        let (met, _) = WaitEvaluator::evaluate(&anchored, &t, &baselines(), "fp0");
        assert!(met);
    }

    #[test]
    fn command_output_only_matches_within_the_command_window() {
        let s = screen(&["BUILD OK"]);
        let mut t = tick(&s);
        let cond = WaitCond::CommandOutput {
            text: "BUILD OK".into(),
            after_command_seq: Some(1),
        };
        // Command 2 running: inside the window (2-1 == 1).
        t.command_seq = 2;
        t.command_running = true;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(met);
        // Command 3 (a LATER command): the earlier command's output can
        // no longer satisfy this wait.
        t.command_seq = 3;
        let (met, _) = WaitEvaluator::evaluate(&cond, &t, &baselines(), "fp0");
        assert!(!met, "a token from an earlier command must not match");
    }
}
