//! Error model. Hermes must never regex-match prose to learn whether an
//! operation failed, so we expose stable machine-readable categories
//! (spec section 2 / Microsoft tui-test error taxonomy).

use rmcp::model::ErrorCode;
use rmcp::ErrorData;

/// Stable error categories returned across the MCP surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Success,
    AssertionFailed,
    InvalidRequest,
    NoSession,
    /// A semantic action target failed to resolve: `ambiguous_target`,
    /// `target_not_found`, or a verb/kind mismatch (Wave D item 31). The
    /// agent can self-correct from the payload (`details.candidates` /
    /// `details.matches` list the resolutions) — this is not a malformed
    /// request, it is a refinement loop.
    TargetError,
    BackendError,
    InternalError,
    Unsupported,
    /// A human holds the control lease on this session (Wave G item 76):
    /// machine-driving tools refuse to act while the lease is valid. The
    /// agent can retry after `ttl_ms` elapses or ask for the lease back.
    ControlLeased,
    /// An expected-state guard refused the action (re-review P0.9): the
    /// screen changed between the caller's observation and its action.
    /// The payload names expected vs actual so the agent can re-observe
    /// and re-decide — retrying blind would misdirect input.
    StaleState,
    /// The current run is closed (`tui_run close`): driving tools refuse to
    /// act and the run no longer accepts evidence. The agent can resume the
    /// run or start a fresh one — retrying against the closed run would
    /// corrupt it as an evidence bundle (review P0.1).
    RunClosed,
    /// The session is healthy but mid-job: its actor mailbox was full when
    /// a non-blocking call tried to enqueue (audit finding 12). This is
    /// TEMPORARY by definition — the session exists and will drain — so it
    /// is not `no_session`. The payload carries `retry_after_ms`; the agent
    /// retries the identical call after the backoff instead of concluding
    /// the session is gone.
    SessionBusy,
}

impl ErrorCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCategory::Success => "success",
            ErrorCategory::AssertionFailed => "assertion_failed",
            ErrorCategory::InvalidRequest => "invalid_request",
            ErrorCategory::NoSession => "no_session",
            ErrorCategory::TargetError => "target_error",
            ErrorCategory::BackendError => "backend_error",
            ErrorCategory::InternalError => "internal_error",
            ErrorCategory::Unsupported => "unsupported",
            ErrorCategory::ControlLeased => "control_leased",
            ErrorCategory::StaleState => "stale_state",
            ErrorCategory::RunClosed => "run_closed",
            ErrorCategory::SessionBusy => "session_busy",
        }
    }
}

/// A uniform result envelope. Every tool returns `{ "category": ..., "error"?, ... }`
/// so the agent can branch on `category` without string matching.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Envelope<T> {
    pub category: ErrorCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured remediation payload (review §14): the machine-usable
    /// fields the prose `error` also names — `retry_after_ms` at the lease
    /// gate, `expected`/`actual` for stale-state, `candidates` for target
    /// resolution, `alternatives` for unsupported paths. Optional and
    /// free-form so a category can carry exactly what its remediation
    /// needs; agents branch on `category`, then read this without
    /// regex-matching the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> Envelope<T> {
    pub fn ok(data: T) -> Self {
        Envelope {
            category: ErrorCategory::Success,
            error: None,
            details: None,
            data: Some(data),
        }
    }
    pub fn fail(cat: ErrorCategory, msg: impl Into<String>) -> Envelope<()> {
        Envelope {
            category: cat,
            error: Some(msg.into()),
            details: None,
            data: None,
        }
    }
    /// Fail with a structured remediation payload alongside the prose.
    pub fn fail_with(
        cat: ErrorCategory,
        msg: impl Into<String>,
        details: serde_json::Value,
    ) -> Envelope<()> {
        Envelope {
            category: cat,
            error: Some(msg.into()),
            details: Some(details),
            data: None,
        }
    }
    /// Render to a JSON string for the MCP tool result.
    pub fn to_json(&self) -> String
    where
        T: serde::Serialize,
    {
        match serde_json::to_string(self) {
            Ok(s) => s,
            Err(e) => format!(
                "{{\"category\":\"{}\",\"error\":\"envelope serialize failed: {}\"}}",
                self.category.as_str(),
                e
            ),
        }
    }
}

/// Convert our internal result into an rmcp [`ErrorData`] (for hard failures)
/// or a successful `CallToolResult` carrying an envelope string.
pub fn to_error_data(cat: ErrorCategory, msg: impl Into<String>) -> ErrorData {
    let code = match cat {
        ErrorCategory::InvalidRequest => ErrorCode::INVALID_PARAMS,
        ErrorCategory::NoSession => ErrorCode::INVALID_PARAMS,
        ErrorCategory::AssertionFailed => ErrorCode::INVALID_PARAMS,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    ErrorData::new(code, msg.into(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review §14: the structured remediation payload rides beside the
    /// prose, round-trips, and stays absent when there is nothing to say.
    #[test]
    fn details_payload_round_trips_and_stays_optional() {
        let plain = Envelope::<()>::fail(ErrorCategory::StaleState, "focus moved");
        assert!(plain.details.is_none());
        let json = plain.to_json();
        assert!(!json.contains("details"), "absent details must not appear");

        let rich = Envelope::<()>::fail_with(
            ErrorCategory::ControlLeased,
            "session 's' is leased",
            serde_json::json!({
                "session": "s",
                "holder": "human",
                "retry_after_ms": 1234,
            }),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&rich.to_json()).expect("envelope parses");
        assert_eq!(parsed["category"], "control_leased");
        assert_eq!(parsed["details"]["retry_after_ms"], 1234);
        assert_eq!(parsed["details"]["holder"], "human");
    }
}
