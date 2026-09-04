//! Wait and assert execution: the shared `WaitCond`/budget semantics every
//! caller (MCP `tui_wait`/`tui_assert`, ScenarioRunner, audit drivers) must
//! agree on.
//!
//! Split from the former single-file `execution` (review §15 god-object
//! residue).

use crate::backend::{WaitCond, WaitOutcome};
use crate::screen::ScreenState;
use crate::session::state::Session;

/// Execute one wait. Thin by design: the point is that every caller uses the
/// same `WaitCond` construction and budget semantics.
pub fn execute_wait(
    session: &mut Session,
    cond: WaitCond,
    budget_ms: u64,
) -> Result<WaitOutcome, anyhow::Error> {
    session.wait(cond, budget_ms)
}

/// Re-review item 17: wait for a declarative event predicate over the
/// converged event history. Polls the session's event queue (which folds in
/// reader-thread bytes and native events on each observe) until an event
/// matching the predicate arrives or the budget expires. Returns the
/// matched event's seq (and the queue's last seq) so the caller can pin the
/// moment it waited for.
pub fn execute_wait_event(
    session: &mut Session,
    predicate: &crate::mcp::params::EventPredicate,
    budget_ms: u64,
) -> Result<WaitEventOutcome, anyhow::Error> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
    loop {
        session.poll_native();
        let matched = session
            .all_events()
            .into_iter()
            .find(|ev| predicate.matches(ev));
        if let Some(ev) = matched {
            return Ok(WaitEventOutcome {
                met: true,
                matched_seq: ev.seq,
                matched_at: ev.at,
                last_seq: session.event_queue_last_seq(),
                elapsed_ms: std::time::Instant::now()
                    .duration_since(deadline - std::time::Duration::from_millis(budget_ms))
                    .as_millis() as u64,
            });
        }
        if std::time::Instant::now() >= deadline {
            return Ok(WaitEventOutcome {
                met: false,
                matched_seq: 0,
                matched_at: 0,
                last_seq: session.event_queue_last_seq(),
                elapsed_ms: budget_ms,
            });
        }
        // A brief observe keeps the queue moving (bytes + native frames
        // fold in here) without busy-spinning the PTY.
        let _ = session.observe(20);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// The result of [`execute_wait_event`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct WaitEventOutcome {
    pub met: bool,
    pub matched_seq: u64,
    pub matched_at: u64,
    pub last_seq: u64,
    pub elapsed_ms: u64,
}

/// Execute one assertion via the shared assertion runner. The MCP layer and
/// the ScenarioRunner must agree on what `focus` or `exit_code` means.
pub fn execute_assert(
    params: &crate::mcp::params::TuiAssertParams,
    screen: &ScreenState,
) -> (bool, String, Option<crate::error::ErrorCategory>) {
    crate::mcp::helpers::run_assertion(params, screen)
}
