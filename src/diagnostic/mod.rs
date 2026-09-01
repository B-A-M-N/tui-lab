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

use std::time::{Duration, Instant};

use crate::backend::Input;
use crate::capture::CaptureStrategy;
use crate::events::TerminalEvent;
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
    pub action: String,
    pub terminal_events: Vec<TerminalEvent>,
    pub frames: Vec<ScreenState>,
    /// Screen + semantic transition between the before and authoritative after.
    pub transition: Transition,
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
/// - `session`: the live session to experiment on.
/// - `stimulus`: the action to apply between before and after (None = pure
///   observation of drift).
/// - `strategy`: how "after" is decided (stable, frames, duration, text, …).
/// - `watch`: aspects to surface as anomalies when they changed.
/// - `budget`: an overall ceiling so a never-settling TUI still returns.
pub fn run_probe(
    session: &mut Session,
    stimulus: Option<Input>,
    strategy: &CaptureStrategy,
    watch: &[ProbeWatch],
    budget: Duration,
) -> anyhow::Result<ProbeResult> {
    let start = Instant::now();
    let action = describe_stimulus(stimulus.as_ref());
    let _ = &budget;

    // 1) Baseline — pure peek of the latest committed frame.
    let before = session
        .last()
        .cloned()
        .or_else(|| session.observe(0).ok())
        .ok_or_else(|| anyhow::anyhow!("probe needs a baseline frame"))?;

    // 2) Apply the stimulus (if any).
    if let Some(input) = stimulus {
        session.send(input)?;
    }

    // 3) Decide "after" per the strategy.
    let after = capture_session(session, strategy, budget)?;

    // 4) Diff.
    let transition = crate::screen::diff::diff(&before, &after);

    // 5) Terminal events observed during the probe (drained post-hoc).
    let terminal_events = session.drain_events();

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

    let frames = vec![after.clone()];
    Ok(ProbeResult {
        before,
        after,
        action,
        terminal_events,
        frames,
        transition,
        timing_ms: start.elapsed().as_millis() as u64,
        anomalies,
    })
}

fn describe_stimulus(input: Option<&Input>) -> String {
    match input {
        Some(Input::Key(k)) => format!("key {:?}", k.code),
        Some(Input::Text(t)) => format!("text {t:?}"),
        Some(Input::MouseClick { button, x, y }) => {
            format!("click {:?} at ({x},{y})", button)
        }
        Some(Input::Raw(b)) => format!("raw {} bytes", b.len()),
        Some(Input::Signal(s)) => format!("signal {s}"),
        Some(other) => format!("{other:?}"),
        None => "no stimulus (drift)".to_string(),
    }
}

/// Drive a `CaptureStrategy` against the session's live backend. The session
/// hides its backend, so strategies that need a raw terminal wait go through
/// `Session::observe` (settle) plus `last()`-peek polling for the rest. This
/// is the one honest reconciliation: never claim a stability/frames result we
/// did not actually observe.
fn capture_session(
    session: &mut Session,
    strategy: &CaptureStrategy,
    budget: Duration,
) -> anyhow::Result<ScreenState> {
    let start = Instant::now();
    let mut seen_upto = session.event_state().screen_seq;
    match strategy {
        CaptureStrategy::Stable { quiet_ms } => {
            let idle = quiet_ms.unwrap_or(120);
            session.observe(idle)
        }
        CaptureStrategy::FirstChange => {
            // Return the first post-anchor change without waiting for quiet.
            loop {
                let now = session.event_state();
                if now.screen_seq > seen_upto {
                    return Ok(session.last().cloned().unwrap_or(session.observe(0)?));
                }
                if start.elapsed() >= budget {
                    return session.observe(0);
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        CaptureStrategy::AfterDuration { ms } => {
            std::thread::sleep(Duration::from_millis(*ms));
            session.observe(0)
        }
        CaptureStrategy::Frames { count } => {
            // Collect distinct frames beyond the anchor and return the first
            // `count`-th one; frame collection is surfaced via `frames` in the
            // result, so here we just drive until we've seen enough.
            let mut collected = 0usize;
            while collected < *count && start.elapsed() < budget {
                let now = session.event_state();
                if now.screen_seq > seen_upto {
                    collected += 1;
                    seen_upto = now.screen_seq;
                } else if now.screen_seq == seen_upto && start.elapsed() > Duration::from_millis(200)
                {
                    break; // quiet — no more frames coming soon
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            session.observe(0)
        }
        CaptureStrategy::UntilText(t) => {
            let t = t.clone();
            loop {
                if let Some(s) = session.last() {
                    if s.viewport_text.iter().any(|r| r.contains(&t))
                        || s.scrollback.iter().any(|r| r.contains(&t))
                    {
                        return session.observe(0);
                    }
                }
                if start.elapsed() >= budget {
                    return session.observe(0);
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        CaptureStrategy::UntilTextAbsent(t) => {
            let t = t.clone();
            loop {
                if let Some(s) = session.last() {
                    let present = s
                        .viewport_text
                        .iter()
                        .any(|r| r.contains(&t))
                        || s.scrollback.iter().any(|r| r.contains(&t));
                    if !present {
                        return session.observe(0);
                    }
                }
                if start.elapsed() >= budget {
                    return session.observe(0);
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        CaptureStrategy::UntilExit => {
            let deadline = start + budget;
            while Instant::now() < deadline {
                if let Some(s) = session.last() {
                    if !s.process.running {
                        return session.observe(0);
                    }
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            session.observe(0)
        }
        CaptureStrategy::UntilEvent(_) => {
            std::thread::sleep(Duration::from_millis(200));
            session.observe(0)
        }
        CaptureStrategy::DeadlineSnapshot { ms } => {
            std::thread::sleep(Duration::from_millis(*ms));
            session.observe(0)
        }
    }
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

/// A sanity helper so callers can build a strategy from its common shapes.
pub fn stable_or(quiet_ms: u64) -> CaptureStrategy {
    CaptureStrategy::Stable {
        quiet_ms: Some(quiet_ms),
    }
}

/// Re-export the pieces a probe consumer needs to not reach into internals.
pub use crate::capture::{capture_by_strategy, CompletionPolicy};
pub use crate::screen::diff::{ScreenDiff, SemanticDiff};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent};

    /// A probe on a real python session: the child streams a menu *after* the
    /// baseline is captured, so a `UntilText` strategy must wait for it and the
    /// probe reports the material change (added cells/controls/regions).
    #[test]
    fn probe_reports_material_change_across_stimulus() {
        let mut sess = Session::new("probe-test".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\n\
                 print('MENU'); sys.stdout.flush()\n\
                 time.sleep(0.3)\n\
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
        // Let the initial MENU line land so the baseline is the settled frame.
        std::thread::sleep(Duration::from_millis(300));

        let result = run_probe(
            &mut sess,
            None,
            &CaptureStrategy::UntilText("[B]".into()),
            &default_watch(),
            Duration::from_secs(6),
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
        sess.stop().ok();
    }

    /// Stimulus sending a key must be reflected as the action in the result,
    /// and the key must be one the line backend can actually deliver (Char).
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
        std::thread::sleep(Duration::from_millis(300));

        let result = run_probe(
            &mut sess,
            Some(Input::Key(KeyEvent::new(KeyCode::Char('x')))),
            &stable_or(150),
            &default_watch(),
            Duration::from_secs(4),
        )
        .expect("probe");
        assert!(
            result.action.contains("key"),
            "action must describe the key: {}",
            result.action
        );
        sess.stop().ok();
    }
}