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
    /// agent can self-correct from the message (matches/candidates are
    /// attached) — this is not a malformed request, it is a refinement loop.
    TargetError,
    BackendError,
    InternalError,
    Unsupported,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> Envelope<T> {
    pub fn ok(data: T) -> Self {
        Envelope {
            category: ErrorCategory::Success,
            error: None,
            data: Some(data),
        }
    }
    pub fn fail(cat: ErrorCategory, msg: impl Into<String>) -> Envelope<()> {
        Envelope {
            category: cat,
            error: Some(msg.into()),
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
/// or a successful [`CallToolResult`] carrying an envelope string.
pub fn to_error_data(cat: ErrorCategory, msg: impl Into<String>) -> ErrorData {
    let code = match cat {
        ErrorCategory::InvalidRequest => ErrorCode::INVALID_PARAMS,
        ErrorCategory::NoSession => ErrorCode::INVALID_PARAMS,
        ErrorCategory::AssertionFailed => ErrorCode::INVALID_PARAMS,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    ErrorData::new(code, msg.into(), None)
}
