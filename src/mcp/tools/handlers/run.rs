//! tui_run: run lifecycle, persistence, diagnosis packets, registry context.
//!
//! Formerly one 600-line match (god-object round 2, G5b): the dispatch
//! stays here; each action family lives in its own child module —
//! [`lifecycle`] (new/close/resume: arms that replace or retire the live
//! run), [`persist`] (persist/list: evidence to disk and what already is),
//! [`diagnostics`] (status/context/diagnose/bundle: read-shaped arms).

pub(crate) mod diagnostics;
pub(crate) mod lifecycle;
pub(crate) mod persist;

use crate::error::ErrorCategory;
use crate::mcp::helpers::err;
use crate::mcp::params::*;
use crate::mcp::tools::TuiLabServer;

/// Body of `tui_run` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_run(
    s: &TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiRunParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::RunAction as RA;
    let Some(run_action) = (match &p.action {
        crate::mcp::params::Known::Known(a) => Some(*a),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown run action '{}' (expected one of: {})",
                    o,
                    <RA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    match run_action {
        RA::New => lifecycle::new(s, p).await,
        RA::Close => lifecycle::close(s, p).await,
        RA::Resume => lifecycle::resume(s, p).await,
        RA::Persist => persist::persist(s, p),
        RA::List => persist::list(s, p),
        RA::Status => diagnostics::status(s),
        RA::Context => diagnostics::context(),
        RA::Diagnose => diagnostics::diagnose(s),
        RA::Bundle => diagnostics::bundle(s, p),
    }
}
