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

use crate::backend::{Input, WaitCond, WaitOutcome};
use crate::screen::diff::Transition;
use crate::screen::ScreenState;
use crate::session::state::Session;

/// One settled interaction: what was sent, whether the screen settled, and
/// the before/after transition. MCP, scenarios, audits, and exploration all
/// consume this shape rather than improvising their own.
#[derive(Debug, Clone)]
pub struct InteractionTransaction {
    /// The action name (e.g. "key", "type", "mouse_click").
    pub action: String,
    /// Whether the post-action settle wait actually met its condition.
    pub settled: bool,
    /// Why the settle resolved (or timed out).
    pub settle_reason: Option<String>,
    pub before: ScreenState,
    pub after: ScreenState,
    pub transition: Transition,
    pub elapsed_ms: u64,
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
    action_name: &str,
    input: Input,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
) -> Result<InteractionTransaction, anyhow::Error> {
    let before = match session.last().cloned() {
        Some(s) => s,
        None => session.observe(0)?,
    };
    let baseline = session.event_state();

    session.send(input)?;

    let (settled, settle_reason, elapsed_ms) = if no_wait {
        (true, Some("no_wait".to_string()), 0)
    } else {
        let budget = settle_budget_ms.max(quiet_ms.saturating_add(1000));
        let outcome = session.wait_after(
            baseline,
            WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(quiet_ms),
                after_screen_seq: None, // wait_after fills the anchor
            },
            budget,
        )?;
        let reason = format!("{:?}", outcome.reason);
        (outcome.met, Some(reason), outcome.elapsed_ms)
    };

    let after = session.observe(quiet_ms)?;
    let transition = crate::screen::diff(&before, &after);

    Ok(InteractionTransaction {
        action: action_name.to_string(),
        settled,
        settle_reason,
        before,
        after,
        transition,
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
            args: vec!["-c".into(), "print('go'); import time; time.sleep(10)".into()],
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
            "key",
            Input::Key(crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x'))),
            60,
            1000,
            false,
        )
        .expect("execute act");
        assert!(tx.settled, "simple echo app must settle: {:?}", tx.settle_reason);
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
            "key",
            Input::Key(crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('y'))),
            60,
            1000,
            true,
        )
        .expect("execute act");
        assert!(tx.settled);
        assert_eq!(tx.settle_reason.as_deref(), Some("no_wait"));
        assert_eq!(tx.elapsed_ms, 0);
    }
}
