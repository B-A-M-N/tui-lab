//! MCP adapter for the central machine-driving boundary (audit P0-1).
//!
//! The pipeline itself lives at [`crate::execution::drive`] so every
//! driver — MCP handlers, the scenario runner, exploration — shares it.
//! This module adds the one layer the engine core cannot carry: the
//! human-control-lease authorization check and the mapping of execution
//! failures onto the MCP envelope error model.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_with_details, lease_refused};

pub(crate) use crate::execution::drive::{act_request_json, ScenarioCapture};

/// Recover the guard's structured `stale_state` payload from the anyhow
/// error [`crate::execution::execute_act_with_guard`] wraps it in. Returns
/// `None` for any other failure. Parsing our own structured marker is the
/// one deliberate string hop left at this boundary — the alternative was
/// widening the execution API to carry the JSON typed, which the review
/// declined as churn.
pub(crate) fn guard_stale_details(e: &anyhow::Error) -> Option<serde_json::Value> {
    let msg = e.to_string();
    let rest = msg.strip_prefix("stale_state: ")?;
    serde_json::from_str(rest).ok()
}

/// Map a canonical-execution error onto the envelope error model: a guard
/// refusal is `stale_state` with the guard's check/expected/actual riding
/// in `details`; anything else is a backend failure.
pub(crate) fn execution_error(e: anyhow::Error) -> rmcp::model::CallToolResult {
    if let Some(stale) = guard_stale_details(&e) {
        let summary = stale
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("expected state no longer holds")
            .to_string();
        return err_with_details(ErrorCategory::StaleState, summary, stale);
    }
    err(ErrorCategory::BackendError, e.to_string())
}

/// Everything one driven act needs, beyond the action itself.
pub(crate) struct DriveSpec<'a> {
    pub action: &'a crate::execution::CanonicalAction,
    pub quiet_ms: u64,
    pub budget_ms: u64,
    pub no_wait: bool,
    pub visibility: crate::execution::InputVisibility,
    pub completion: crate::capture::CompletionPolicy,
    pub guard: Option<&'a crate::execution::MutationGuard>,
    /// `None` = do not touch scenario recordings (e.g. internal plan
    /// steps that are not themselves caller actions).
    pub scenario: Option<ScenarioCapture>,
}

/// The evidence one driven act produced.
pub(crate) struct DriveOutcome {
    /// The full interaction transaction (frames, settle, transition,
    /// render evidence).
    pub tx: crate::execution::InteractionTransaction,
    /// Frame references: `{"before": "frame:N", "after": "frame:M"}`.
    pub frames: serde_json::Value,
}

/// THE driving pipeline, MCP flavor: lease authorization (Wave G item 76,
/// applied at the ONE place — a live human control lease blocks machine
/// driving from every driver, not only tui_act), then the shared
/// execution/evidence core, then envelope error mapping. Runs inside the
/// session's actor (call it from a `with_sess` closure only). Returns the
/// outcome, or the envelope error the driver must surface unchanged
/// (lease refusal, stale_state, backend failure).
pub(crate) fn drive(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>,
    spec: DriveSpec<'_>,
) -> Result<DriveOutcome, rmcp::model::CallToolResult> {
    if let Some(refused) = lease_refused(sess) {
        return Err(refused);
    }
    let core = crate::execution::CoreDriveSpec {
        action: spec.action,
        quiet_ms: spec.quiet_ms,
        budget_ms: spec.budget_ms,
        no_wait: spec.no_wait,
        visibility: spec.visibility,
        completion: spec.completion,
        guard: spec.guard,
        scenario: spec.scenario,
    };
    crate::execution::drive_pipeline(sess, run, core)
        .map(|o| DriveOutcome {
            tx: o.tx,
            frames: o.frames,
        })
        .map_err(execution_error)
}
