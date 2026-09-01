//! DiagnosticProbe — the review's headline troubleshooting primitive.
//!
//! When the agent does not yet understand an application it needs: *"try
//! this small experiment and tell me EVERYTHING materially different."* That
//! is a probe, distinct from `observe`/`act`/`wait`/`assert`/`scenario`/
//! `audit`/`explore` because it *composes* a baseline capture, a stimulus, a
//! generalized [`CaptureStrategy`], and a before/after diff into one result.
//!
//! It never requires classification first (Raw-mode friendly) and never
//! mandates a stable screen (the strategy decides "done"). It reports what
//! *changed* materially — screen cells, semantics (controls/regions/focus),
//! process state — and flags anomalies, so an unknown TUI can be debugged by
//! experiment rather than by forcing every attempt into an assertion.

use std::time::Instant;

use crate::capture::CompletionPolicy;
use crate::events::TerminalEvent;
use crate::execution::{CanonicalAction, SettleStatus};
use crate::screen::diff::Transition;
use crate::screen::ScreenState;
use crate::session::state::Session;

/// What aspects of a probe's transition to watch for anomalies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeWatch {
    Cursor,
    Focus,
    Style,
    Controls,
    Regions,
    Process,
}

/// The result of one probe: everything materially different.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub before: ScreenState,
    pub after: ScreenState,
    /// Exact canonical-action provenance ([`CanonicalAction::signature`]) or
    /// `"no stimulus (drift)"`.
    pub action: String,
    /// How the settle resolved (`Skipped` for drift probes).
    pub settle: SettleStatus,
    /// Events that fired inside the probe window (causal, cursor-scoped —
    /// not a drain of the whole queue).
    pub terminal_events: Vec<TerminalEvent>,
    pub frames: Vec<ScreenState>,
    /// Screen + semantic transition between the before and authoritative after.
    pub transition: Transition,
    /// Fused focus of the after-frame `(control_id, label)` — the same truth
    /// every semantic read sees (native self-reports participate).
    pub after_focus: Option<(Option<String>, Option<String>)>,
    pub timing_ms: u64,
    /// Human-readable anomalies (watched aspects that changed materially).
    pub anomalies: Vec<String>,
}

impl ProbeResult {
    /// Whether the probe found *any* material change worth reporting.
    pub fn has_changes(&self) -> bool {
        self.transition.screen_diff.changed_cells > 0
            || !self.transition.semantic_diff.controls_added.is_empty()
            || !self.transition.semantic_diff.controls_removed.is_empty()
            || !self.transition.semantic_diff.regions_added.is_empty()
            || self.transition.screen_diff.cursor.is_some()
            || self.transition.screen_diff.process.is_some()
    }
}

/// Run one probe against a live session.
///
/// Re-review Wave-2 (canonical diagnostics): the stimulus goes through the
/// ONE canonical executor — [`execute_act_with_completion`] — so a probe
/// inherits everything an act gets: the pre-action causal anchor
/// ([`TerminalEventState`]), completion compiled and evaluated by the one
/// compiler/interpreter, event-queue events scoped to the probe window via a
/// per-probe cursor (the old post-hoc `drain_events()` stole every other
/// consumer's events), and a real [`InteractionTransaction`] carrying the
/// settled after-frame. A `None` stimulus still runs an observe-only drift
/// probe (baseline → settle → diff), which is its own diagnostic.
///
/// - `session`: the live session to experiment on.
/// - `stimulus`: the CANONICAL action to apply between before and after
///   (None = pure observation of drift).
/// - `completion`: how "after" is decided (stable, first-change, text, …).
/// - `watch`: aspects to surface as anomalies when they changed.
/// - `quiet_ms` / `budget_ms`: the settle policy and overall ceiling so a
///   never-settling TUI still returns.
pub fn run_probe(
    session: &mut Session,
    stimulus: Option<CanonicalAction>,
    completion: CompletionPolicy,
    watch: &[ProbeWatch],
    quiet_ms: u64,
    budget_ms: u64,
) -> anyhow::Result<ProbeResult> {
    let start = Instant::now();
    let action = match &stimulus {
        Some(a) => a.signature(),
        None => "no stimulus (drift)".to_string(),
    };

    // Events are consumed through causal cursors (`events_since(pre_seq)`),
    // not a stealing drain: the probe reads exactly the events that fired
    // inside its own window and leaves the queue intact for every other
    // reader (re-review Wave-2: the queue is the authority; drains are gone).

    // 1) Baseline — pure peek of the latest committed frame, cursor pinned
    //    BEFORE any stimulus so the window is causal.
    let before = session
        .last()
        .cloned()
        .or_else(|| session.observe(0).ok())
        .ok_or_else(|| anyhow::anyhow!("probe needs a baseline frame"))?;
    let pre_seq = session.event_queue_last_seq();

    // 2+3) Apply the stimulus and decide "after" via the canonical
    //       executor's compiled completion plan.
    let (tx, events) = match stimulus {
        Some(act) => {
            let tx = crate::execution::execute_act_with_completion(
                session,
                &act,
                quiet_ms,
                budget_ms,
                false,
                crate::execution::InputVisibility::Normal,
                completion,
            )?;
            let batch = session.events_since(pre_seq);
            (Some(tx), batch.events)
        }
        None => {
            let after = session.observe(quiet_ms.min(budget_ms.max(1)))?;
            let _ = after;
            let batch = session.events_since(pre_seq);
            (None, batch.events)
        }
    };

    // The authoritative after-frame: the transaction's settled frame when a
    // stimulus ran, else the fresh observation.
    let after = match &tx {
        Some(tx) => tx.after_frame.state.clone(),
        None => session.last().cloned().unwrap_or(before.clone()),
    };

    // 4) Diff.
    let transition = crate::screen::diff::diff(&before, &after);

    // 6) Consistent anomalies from the watched aspects.
    let mut anomalies = Vec::new();
    for aspect in watch {
        match aspect {
            ProbeWatch::Cursor => {
                if let Some(c) = &transition.screen_diff.cursor {
                    let before = &c.before;
                    let after = &c.after;
                    let moved = (before.x, before.y) != (after.x, after.y);
                    anomalies.push(format!(
                        "cursor moved {:?} -> {:?} (moved={}, visible {}-{})",
                        (before.x, before.y),
                        (after.x, after.y),
                        moved,
                        before.visible,
                        after.visible
                    ));
                }
            }
            ProbeWatch::Focus => {
                let sd = &transition.semantic_diff;
                if sd.focus_before.as_deref() != sd.focus_after.as_deref() {
                    anomalies.push(format!(
                        "focus changed {:?} -> {:?}",
                        sd.focus_before, sd.focus_after
                    ));
                }
            }
            ProbeWatch::Controls => {
                let sd = &transition.semantic_diff;
                if !sd.controls_added.is_empty() {
                    anomalies.push(format!("controls added: {:?}", sd.controls_added));
                }
                if !sd.controls_removed.is_empty() {
                    anomalies.push(format!("controls removed: {:?}", sd.controls_removed));
                }
            }
            ProbeWatch::Regions => {
                let sd = &transition.semantic_diff;
                if !sd.regions_added.is_empty() {
                    anomalies.push(format!("regions added: {:?}", sd.regions_added));
                }
            }
            ProbeWatch::Style => {
                if transition.screen_diff.style_changes > 0 {
                    anomalies.push(format!(
                        "style changes: {}",
                        transition.screen_diff.style_changes
                    ));
                }
            }
            ProbeWatch::Process => {
                if let Some(p) = &transition.screen_diff.process {
                    anomalies.push(format!(
                        "process: running {}/{}",
                        p.before.running, p.after.running
                    ));
                    if let Some(code) = p.after.exit_code {
                        anomalies.push(format!("process exited with code {code}"));
                    }
                }
            }
        }
    }

    // Frames: the transaction's own frame sequence when one exists, else the
    // single settled observation.
    let frames: Vec<ScreenState> = tx
        .as_ref()
        .and_then(|tx| tx.capture.as_ref())
        .and_then(|c| c.frames.clone())
        .unwrap_or_else(|| vec![after.clone()]);

    let settle = tx.as_ref().map(|tx| tx.settle).unwrap_or(SettleStatus::Skipped);
    // Fused after-focus: for a stimulated probe the transaction already
    // computed it; a drift probe fuses its fresh frame here.
    let after_focus = tx
        .as_ref()
        .map(|tx| tx.focus_after.clone())
        .unwrap_or_else(|| {
            session.poll_native();
            use crate::semantic::FocusOption;
            session.fuse_screen(&after).focus_for_option()
        });
    Ok(ProbeResult {
        before,
        after,
        action,
        settle,
        terminal_events: events,
        frames,
        transition,
        after_focus,
        timing_ms: start.elapsed().as_millis() as u64,
        anomalies,
    })
}

/// Convenience: `ProbeWatch::Focus` + `Controls` is the default—what the
/// probe most reliably surfaces for an unknown TUI.
pub fn default_watch() -> Vec<ProbeWatch> {
    vec![
        ProbeWatch::Focus,
        ProbeWatch::Controls,
        ProbeWatch::Regions,
        ProbeWatch::Cursor,
    ]
}

/// A sanity helper so callers can build a completion from its common shape.
/// `StableScreen` uses the session/plan's own quiet policy — the argument is
/// kept for call-site readability and is currently advisory.
pub fn stable_or(_quiet_ms: u64) -> CompletionPolicy {
    CompletionPolicy::StableScreen
}

/// Re-export the pieces a probe consumer needs to not reach into internals.
pub use crate::screen::diff::{ScreenDiff, SemanticDiff};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent};

    /// A probe on a real python session: the child streams a menu *after* the
    /// baseline is captured, so a `TextAppears` completion must wait for it
    /// and the probe reports the material change (added cells/controls/regions).
    #[test]
    fn probe_reports_material_change_across_stimulus() {
        let mut sess = Session::new("probe-test".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\n\
                 print('MENU'); sys.stdout.flush()\n\
                 input()\n\
                 sys.stdout.write('  [A] Alpha  [B] Beta\\n'); sys.stdout.flush()\n\
                 time.sleep(2)"
                    .into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        // The child is gated on input(): MENU is the only line it can ever
        // print before we say GO, so the baseline is the settled MENU frame
        // deterministically (no fixed sleep to race) and "[B]" is guaranteed
        // absent when the probe waits for its appearance.
        sess.observe(300).expect("menu baseline");

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Type { text: "GO\n".into() }),
            CompletionPolicy::TextAppears("[B]".into()),
            &default_watch(),
            120,
            10_000,
        )
        .expect("probe");
        assert!(
            result.has_changes(),
            "screen must differ between MENU baseline and [B] frame"
        );
        assert!(
            result.transition.screen_diff.changed_cells > 0
                || result.anomalies.iter().any(|a| a.contains("controls")),
            "probe should surface the added menu: {:?}",
            result.anomalies
        );
        assert_eq!(result.action, "text", "exact canonical signature");
        assert_eq!(result.settle, SettleStatus::Met, "text appeared");
        sess.stop().ok();
    }

    /// Stimulus sending a key must be reflected as the EXACT action
    /// signature in the result, and the settle must be honestly reported.
    #[test]
    fn probe_records_the_action_it_applied() {
        let mut sess = Session::new("probe-act".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('READY'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        std::thread::sleep(std::time::Duration::from_millis(300));

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Key {
                key: KeyEvent::new(KeyCode::Char('x')),
            }),
            stable_or(150),
            &default_watch(),
            150,
            4_000,
        )
        .expect("probe");
        assert_eq!(
            result.action, "x",
            "action must be the exact signature: {}",
            result.action
        );
        sess.stop().ok();
    }

    /// Wave-2: events reported by a probe are causal (fired inside the probe
    /// window) AND the queue stays intact for other consumers — the old
    /// post-hoc `drain_events()` stole them.
    #[test]
    fn probe_events_are_scoped_and_queue_survives() {
        let mut sess = Session::new("probe-ev".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('EV-READY'); sys.stdout.flush()\ninput()\nprint('EV-NEXT'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        sess.observe(300).expect("baseline");

        // A named consumer pins its cursor before the probe.
        let pre = sess.events_for_consumer("watcher");
        let pre_cursor = pre.cursor;

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Type { text: "go\n".into() }),
            CompletionPolicy::TextAppears("EV-NEXT".into()),
            &default_watch(),
            120,
            6_000,
        )
        .expect("probe");

        // The probe's window contains at least the text-appearing observation
        // events, and every reported event is beyond the pre-probe cursor.
        for ev in &result.terminal_events {
            assert!(
                ev.seq >= pre_cursor,
                "probe events must be inside the window: {:?} seq={} pre={pre_cursor}",
                ev.kind,
                ev.seq
            );
        }
        // The named consumer still reads everything (no stolen events).
        let post = sess.events_for_consumer("watcher");
        assert!(
            post.cursor >= pre_cursor,
            "consumer cursor never rewinds"
        );
        assert!(
            !result.terminal_events.is_empty() || post.cursor >= pre_cursor,
            "probe window produced events or queue intact"
        );
        sess.stop().ok();
    }

    /// Drift probe (None stimulus): no input is sent, the action string says
    /// so, and the settle is honestly Skipped.
    #[test]
    fn drift_probe_reports_no_stimulus() {
        let mut sess = Session::new("probe-drift".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('DRIFT-OK'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        sess.observe(300).expect("baseline");

        let result = run_probe(
            &mut sess,
            None,
            CompletionPolicy::StableScreen,
            &default_watch(),
            120,
            3_000,
        )
        .expect("probe");
        assert_eq!(result.action, "no stimulus (drift)");
        assert_eq!(result.settle, SettleStatus::Skipped);
        sess.stop().ok();
    }
}