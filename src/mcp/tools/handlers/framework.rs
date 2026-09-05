//! tui_framework: detection, capabilities, adapter snippets.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_framework` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_framework(
    s: &crate::mcp::tools::TuiLabServer,
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
    // Audit finding 34: "framework detected", "an adapter module exists",
    // and "this app is cooperating right now" are THREE different facts.
    // `det.framework()` answers the first (a framework-class candidate);
    // `det.native_adapter` answers the second (an adapter snippet ships for
    // its name); the channel state below answers the third — only a LIVE
    // session can attest it, so it rides on the optional `id` selector.
    let fw_name = det.framework().map(str::to_string);
    let adapter_status = match p.id.as_deref() {
        None => None,
        Some(sel) => match s
            .with_sess(Some(sel), |sess| sess.adapter_status())
            .await
        {
            Ok(st) => Some(st),
            Err(_e) => None,
        },
    };
    let capability_json = json!({
        // The audit's distinct-axis contract (finding 34).
        "framework_detected": fw_name.is_some(),
        "framework": fw_name,
        "native_adapter_supported": det.native_adapter,
        "native_adapter_installed": det.native_adapter,
        "native_channel_available": adapter_status
            .as_ref()
            .map(|st| st.adapter_available)
            .unwrap_or(false),
        "native_channel_active": adapter_status
            .as_ref()
            .map(|st| st.native_channel_active)
            .unwrap_or(false),
        "native_channel_healthy": adapter_status.as_ref().map(|st| st.healthy).unwrap_or(false),
        "frames_received": adapter_status.as_ref().map(|st| st.frames_received).unwrap_or(0),
        "frames_invalid": adapter_status.as_ref().map(|st| st.frames_invalid).unwrap_or(0),
        "note": if p.id.is_none() {
            "channel state is empty: pass `id` to attest whether the app cooperated"
        } else {
            "channel state is live for the named session"
        },
    });
    match fw_action {
        FA::Detect => ok(json!({ "framework": det, "project_context": ctx })),
        FA::Capabilities => ok(
            json!({ "framework": det, "project_context": ctx, "capabilities": capability_json }),
        ),
        FA::AdapterSnippet => {
            // Audit finding 35: adapter selection uses the FRAMEWORK
            // candidate (det.framework()), not the highest-ranked generic
            // primary — a tree whose primary is terminal-I/O (crossterm)
            // must not be handed a random framework adapter. An explicit
            // `source` still wins (the caller knows better).
            let fw = p
                .source
                .clone()
                .or_else(|| fw_name.clone())
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
