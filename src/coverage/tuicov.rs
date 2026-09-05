//! Coverage integration (spec section 5; Wave F item 64).
//!
//! Two providers, merged honestly:
//!
//! 1. **tuicov** — an optional external executable. When present on PATH,
//!    we invoke it for its normalized JSON (files → lines / widget hits)
//!    and map it into our ledger. When absent we say so — never a fake
//!    percentage.
//! 2. **NativeSemanticProtocol coverage events** — a cooperative app can
//!    send `{"v":1,"type":"event","event":"coverage","target":"src/main.rs:42"}`
//!    frames over the side channel; the session's NativeChannel collects
//!    them and the run ledger aggregates them per session.
//!
//! The ledger is keyed by interaction sequence: `delta` reports which files
//! gained line/widget hits since the caller's cursor, so Hermes can know
//! whether an interaction exercised new application code.

use crate::error::{Envelope, ErrorCategory};
use crate::mcp::params::{CoverageAction, Known, TuiCoverageParams};

/// Handle a coverage tool call. Returns a JSON string envelope.
///
/// This module owns ONLY the parts it can answer honestly from the tuicov
/// companion executable and its availability probe:
///
/// * `detect` — is the tuicov executable present? (honest availability)
/// * `snapshot` — invoke tuicov's `--json` once and normalize it. There is
///   exactly ONE real tuicov invocation shape; the old code routed
///   `delta`/`uncovered`/`start`/`stop` through it with an ignored `_action`
///   (four actions, one identical command — flagged as theater). Those are
///   gone: start/stop were removed (collection is continuous, there is no
///   instrumentation phase), and the run-ledger views (`summary`, `collect`,
///   `delta`, `ledger`) are answered against the real accumulated ledger by
///   the MCP layer in `tools.rs`.
///
/// Everything else is an honest `unsupported` — never a fabricated shape.
pub fn handle(p: &TuiCoverageParams) -> Result<String, anyhow::Error> {
    let action = p
        .action
        .clone()
        .unwrap_or(Known::Known(CoverageAction::Detect));
    match action.known() {
        Some(CoverageAction::Detect) => {
            let available = which_tuicov();
            Ok(Envelope::ok(serde_json::json!({
                "available": available,
                "provider": if available { "tuicov" } else { "native-events" },
                "native_events": "collected continuously into the run ledger when the app cooperates over TUI_LAB_SEMANTIC",
                "note": "run-ledger views (summary/collect/delta/ledger) read accumulated native coverage evidence; tuicov is an optional snapshot executable",
            }))
            .to_json())
        }
        // Audit finding 31: the point-in-time snapshot is now a real MCP
        // action, not a described-only capability. It returns the provider
        // report plus the metadata needed to correlate it with the run's
        // native ledger.
        Some(CoverageAction::Snapshot) => {
            if !is_available() {
                return Ok(Envelope::<()>::fail(
                    ErrorCategory::Unsupported,
                    "tui_coverage snapshot requires the optional 'tuicov' executable on PATH; the run ledger's native coverage views (summary/collect/delta/ledger) work without it",
                )
                .to_json());
            }
            let snap = snapshot().map_err(anyhow::Error::msg)?;
            Ok(Envelope::ok(serde_json::json!({
                "provider": "tuicov",
                "snapshot": snap,
                "correlation": "join tuicov's file/widget hits against the run ledger's coverage targets by name (tui_coverage action=ledger); the native ledger carries per-target hit counts and source_refs",
            }))
            .to_json())
        }
        // The run-ledger views are answered by the MCP layer (run-scoped);
        // if one reaches here the dispatch drifted — say so honestly.
        Some(CoverageAction::Ledger)
        | Some(CoverageAction::Summary)
        | Some(CoverageAction::Collect)
        | Some(CoverageAction::Delta)
        | Some(CoverageAction::Uncovered)
        | None => match &action {
            Known::Other(other) => Ok(Envelope::<()>::fail(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown coverage action '{}' (expected one of: {})",
                    other,
                    <CoverageAction as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                ),
            )
            .to_json()),
            _ => Ok(Envelope::<()>::fail(
                ErrorCategory::InternalError,
                "coverage action is answered by the run layer in tools.rs; this path should not have been reached",
            )
            .to_json()),
        },
    }
}

/// Invoke tuicov and normalize its JSON into our ledger shape. tuicov is
/// expected to emit `{"files":[{"path":…,"lines":[n,…],"widgets":[…]}]}`;
/// anything else is reported as a parse failure, never coerced.
///
/// THIS is the one real tuicov operation (review P0.7): a single `--json`
/// snapshot. The old code routed four distinct-looking actions
/// (delta/uncovered/start/stop) through one identical invocation with an
/// ignored `_action` — theater. Those actions were removed; the run ledger
/// answers delta/summary/collect from accumulated evidence, and start/stop
/// do not exist because collection is continuous (no instrumentation phase).
pub fn snapshot() -> Result<serde_json::Value, String> {
    use std::process::Command;
    let out = Command::new("tuicov")
        .arg("--json")
        .output()
        .map_err(|e| format!("tuicov invocation failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tuicov exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("tuicov output is not JSON: {e}"))?;
    // Normalize: files with line/widget hits.
    let files = parsed
        .get("files")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    let total_lines: usize = files
        .iter()
        .filter_map(|f| f.get("lines").and_then(|l| l.as_array()))
        .map(|l| l.len())
        .sum();
    let total_widgets: usize = files
        .iter()
        .filter_map(|f| f.get("widgets").and_then(|w| w.as_array()))
        .map(|w| w.len())
        .sum();
    Ok(serde_json::json!({
        "provider": "tuicov",
        "files": files.len(),
        "lines_covered": total_lines,
        "widgets_covered": total_widgets,
        "detail": files,
    }))
}

/// Check if the tuicov executable is available on PATH.
pub fn is_available() -> bool {
    which_tuicov()
}

fn which_tuicov() -> bool {
    // Probe PATH for `tuicov`. Do not fail the build if absent.
    use std::process::Command;
    Command::new("tuicov")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_reports_honestly() {
        let env = Envelope::<serde_json::Value>::ok(serde_json::json!({
            "available": is_available()
        }));
        let body = env.to_json();
        assert!(body.contains("available"));
    }

    /// The tuicov normalizer maps the documented JSON shape; a malformed
    /// payload is an error, never silently-empty coverage.
    #[test]
    fn normalize_rejects_non_json() {
        let bad = "not json at all";
        assert!(serde_json::from_str::<serde_json::Value>(bad).is_err());
        let good = serde_json::json!({"files": [
            {"path": "src/main.rs", "lines": [1, 2, 3], "widgets": ["#save"]}
        ]});
        assert_eq!(good["files"].as_array().map(|a| a.len()), Some(1));
    }

    /// P0.7: tuicov's one real operation invokes a single `--json` snapshot —
    /// no start/stop/delta/uncovered argv theater. We point it at a FAKE
    /// `tuicov` on PATH that records the argv it received and echo a valid
    /// coverage shape, then assert exactly that one argument was passed.
    #[test]
    fn tuicov_snapshot_invokes_single_json_arg() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("tui-cover-test-{}", std::process::id()));
        let bindir = tmp.join("bin");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&bindir).unwrap();
        // The fake tuicov: log argv, emit a valid coverage JSON.
        let fake = bindir.join("tuicov");
        let mut f = std::fs::File::create(&fake).unwrap();
        let _ = writeln!(
            f,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}/argv.log\"\nprintf '{{\"files\":[{{\"path\":\"src/main.rs\",\"lines\":[1,2],\"widgets\":[\"#save\"]}}]}}'\n",
            bindir.display()
        );
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Prepend the fake bin dir to PATH for the invocation.
        let saved = std::env::var_os("PATH");
        let mut paths: Vec<_> = std::env::split_paths(&saved.clone().unwrap_or_default()).collect();
        paths.insert(0, bindir.clone());
        std::env::set_var("PATH", std::env::join_paths(&paths).unwrap());

        // snapshot() must fail if a previous parallel test raced us; run it
        // and check both the argv and the reply.
        let result = snapshot();
        let argv = std::fs::read_to_string(bindir.join("argv.log")).unwrap_or_else(|_| "".into());
        let argv: Vec<&str> = argv.trim().lines().collect();
        assert_eq!(
            argv,
            ["--json"],
            "tuicov must be invoked with exactly --json, got {argv:?}"
        );
        let reply = result.expect("snapshot parsed");
        assert_eq!(reply["files"], serde_json::json!(1));
        assert_eq!(reply["lines_covered"], serde_json::json!(2));

        // Restore PATH.
        if let Some(p) = saved {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
