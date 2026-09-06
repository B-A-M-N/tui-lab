//! Run persistence + registry surfaces (god-object round 2, G5b):
//! `persist` and `list` — the two `tui_run` arms that put run
//! evidence ON DISK or enumerate what already is. Split out of the
//! former single `tui_run` match; bodies are verbatim.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::TuiRunParams;
use rmcp::serde_json::json;

/// Persist the live run: promote it from ephemeral to a durable
/// artifact tree. Root resolution: explicit `root` wins; else the
/// primary session's LaunchSpec.cwd; else invalid_request — do NOT
/// guess from the server process cwd.
pub(crate) fn persist(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
    let mut run = s.run.lock().unwrap();
    if run.run_dir().is_some() {
        // Audit P1 (response freshness): "already persistent" is not a
        // no-op answer — a run can be persistent yet hold unflushed
        // in-memory state. Flush (best-effort here; a failure is
        // reported, not swallowed) and answer with the CURRENT
        // durability picture, not a bare echo.
        let flush = run.flush().err().map(|e| e.to_string());
        let run_id = run.id().to_string();
        let artifact_root = run.run_dir().map(|d| d.to_string_lossy().to_string());
        let summary = run.status(Vec::new());
        return ok(json!({
            "run_id": run_id,
            "already_persistent": true,
            "artifact_root": artifact_root,
            "flush_error": flush,
            "final": summary,
        }));
    }
    let base: String = match p.root.clone() {
        Some(r) => r,
        None => match run.primary_session_cwd() {
            Some(cwd) => cwd.to_string(),
            None => {
                let run_id = run.id().to_string();
                return err(
                    ErrorCategory::InvalidRequest,
                    format!(
                        "no artifact root resolvable for run {}: pass 'root' explicitly, or start a session whose LaunchSpec.cwd is set",
                        run_id
                    ),
                );
            }
        },
    };
    match run.promote(std::path::Path::new(&base)) {
        Ok(root) => ok(json!({
            "run_id": run.id(),
            "persistent": true,
            "artifact_root": root.to_string_lossy(),
            "promoted_from_ephemeral": true,
        })),
        Err(e) => err(ErrorCategory::InternalError, format!("persist failed: {e}")),
    }
}

/// Wave G item 75: enumerate persisted runs under a resolved
/// root (explicit root, else the primary session's cwd; neither
/// present is invalid_request — the same no-guessing rule persist
/// follows). Corrupt entries are named in place, never dropped.
pub(crate) fn list(
    s: &crate::mcp::tools::TuiLabServer,
    p: TuiRunParams,
) -> rmcp::model::CallToolResult {
    let base: String = match p.root.clone() {
        Some(r) => r,
        None => match s.run.lock().unwrap().primary_session_cwd() {
            Some(cwd) => cwd.to_string(),
            None => {
                return err(
                    ErrorCategory::InvalidRequest,
                    "no root to scan for runs: pass 'root', or start a session whose LaunchSpec.cwd is set",
                )
            }
        },
    };
    match crate::run::RunContext::list_persisted(std::path::Path::new(&base)) {
        Ok(entries) => {
            // Audit P0-45: partition ONCE. The old code drained
            // into `runs` first, so `skipped` was always empty —
            // corrupt entries silently vanished. A directory with
            // no run.json lands here with its `skipped` reason;
            // keep it — corruption is evidence.
            let (runs, skipped): (Vec<_>, Vec<_>) =
                entries.into_iter().partition(|e| e.get("run_id").is_some());
            ok(json!({
                "base": base,
                "runs": runs,
                "skipped": skipped,
                "current_run": s.run.lock().unwrap().id().to_string(),
            }))
        }
        Err(e) => err(ErrorCategory::InvalidRequest, e.to_string()),
    }
}
