//! tui_framework: detection, capabilities, adapter snippets.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_framework` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_framework(
    _s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiFrameworkParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    let cwd = p.cwd.clone().unwrap_or_else(|| ".".into());
    // Resolve the project root through the bounded-upward ProjectLocator
    // BEFORE detection (re-review "frame detection makes a hidden
    // manifest-is-in-cwd assumption"): a monorepo caller passing a nested
    // package dir (`packages/foo/src`) should have the dependency scan run
    // against the package boundary, not the nested dir with no manifest at
    // that exact path. The locator never errors and degrades to `cwd` when
    // no manifest is found, so no-manifest raw projects detect identically.
    let project = crate::session::ProjectLocator::locate(None, &cwd);
    let detect_cwd = project.root().to_string();
    // Item 34: the project context — the package root AND the workspace
    // root — rides on every detect/capabilities response so callers see
    // both boundaries (lockfile/workspace evidence lives at the root,
    // dependency evidence at the package; conflating them hides half).
    let ctx = crate::framework::context::ProjectContext::resolve(&detect_cwd);
    let mut det = crate::framework::detect::detect(&detect_cwd);
    // Surface the resolved root as evidence so the agent sees *which*
    // directory the detection ran against (and can pass an explicit one).
    if project.root() != cwd {
        det.evidence
            .push(format!("resolved project root: {}", project.root()));
    }
    use crate::mcp::params::FrameworkAction as FA;
    let Some(fw_action) = (match &p.action {
        crate::mcp::params::Known::Known(a) => Some(*a),
        crate::mcp::params::Known::Other(o) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown framework action '{}' (expected one of: {})",
                    o,
                    <FA as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            );
        }
    }) else {
        unreachable!()
    };
    match fw_action {
        FA::Detect => ok(json!({ "framework": det, "project_context": ctx })),
        FA::Capabilities => ok(
            json!({ "framework": det, "project_context": ctx, "note": "native probes invoke project-local tooling" }),
        ),
        FA::AdapterSnippet => {
            let fw = p
                .source
                .clone()
                .or_else(|| {
                    // Item 33: adapters exist for FRAMEWORKS; a terminal-I/O
                    // or styling candidate is not an adapter target.
                    det.primary
                        .as_ref()
                        .filter(|c| c.class == "framework")
                        .map(|c| c.name.clone())
                })
                .unwrap_or_else(|| "python".into());
            match crate::framework::adapters::snippet_for(&fw.to_lowercase()) {
                    Some(code) => ok(json!({
                        "framework": fw,
                        "language": if fw == "ratatui" { "rust" } else { "python" },
                        "protocol_version": crate::semantic::native::PROTOCOL_VERSION,
                        "env_var": crate::semantic::native::ENV_VAR,
                        "snippet": code,
                    })),
                    None => err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "no adapter snippet for '{}' (available: ratatui, textual, python/reference)",
                            fw
                        ),
                    ),
                }
        }
    }
}
