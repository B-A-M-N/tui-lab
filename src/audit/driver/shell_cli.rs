//! Shell/CLI family: argv handling, shell-integration, and CLI
//! presentation audits.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::session::state::Session;
use serde_json::json;

use super::shared::{decode_raw, ev_other, ev_other_empty};
use crate::audit::Finding;

/// Item 30 — shell/CLI audit. For line-oriented CLIs (no alternate screen):
/// prompt detection, shell-integration marks (OSC 133), and exit-status
/// reporting. Tells an agent walking into an unfamiliar CLI how to script
/// it and whether its completion signal is trustworthy.
pub fn shell_cli_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "SH-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "shell_cli".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "shell_observe_failed",
                    "session.observe failed at shell/CLI audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };

    let proc_state = session.process();
    let cmd_state = session.backend_command_state();

    // Shell-integration marks: the gold standard for command edges.
    let has_osc133 = cmd_state.is_some();
    if has_osc133 {
        let cs = cmd_state.unwrap();
        findings.push(Finding {
            id: "SH-CMDSTATE".into(),
            rule_id: None,
            severity: "info".into(),
            category: "shell_cli".into(),
            summary: format!(
                "shell-integration marks present: phase={}, running={}, last exit {:?}. Command edges are exact — tui_wait condition=command_done is trustworthy here.",
                cs.phase, cs.running, cs.last_exit
            ),
            evidence: vec![ev_other(
                "command_state",
                "OSC 133 shell-integration timeline",
                json!({
                    "phase": cs.phase,
                    "running": cs.running,
                    "last_exit": cs.last_exit,
                }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Prompt detection: trailing `> ` / `$ ` / `% ` / `# ` on the last
    // non-empty line — the heuristic fallback when no marks exist.
    let last_line = screen
        .viewport_text
        .iter()
        .rev()
        .find(|r| !r.trim().is_empty())
        .map(|r| r.trim_end().to_string());
    let prompt_hint = last_line.as_deref().map(|l| {
        l.ends_with("$ ")
            || l.ends_with("> ")
            || l.ends_with("% ")
            || l.ends_with("# ")
            || l == "$"
            || l == ">"
    });

    if !has_osc133 {
        findings.push(Finding {
            id: "SH-NOMARKS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "shell_cli".into(),
            summary: if prompt_hint.unwrap_or(false) {
                "no shell-integration marks; the last line looks like a prompt — text/regex waits are the only command-completion signal here.".to_string()
            } else {
                "no shell-integration marks and no trailing prompt shape; completion must be inferred from output silence (stable-screen waits).".to_string()
            },
            evidence: vec![ev_other(
                "prompt_heuristic",
                "prompt shape from the last viewport line",
                json!({
                    "last_line": last_line,
                    "prompt_shaped": prompt_hint,
                    "note": "injecting OSC 133 marks (shell integration) makes command_done waits exact",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Exit-status honesty: has the process ended, and is a code visible?
    if !proc_state.running {
        findings.push(Finding {
            id: "SH-EXIT".into(),
            rule_id: None,
            severity: "info".into(),
            category: "shell_cli".into(),
            summary: format!(
                "process has exited (code {:?}, signal {:?}); further input sends will fail.",
                proc_state.exit_code, proc_state.exit_signal
            ),
            evidence: vec![ev_other(
                "process_exit",
                "process state at audit time",
                json!({
                    "exit_code": proc_state.exit_code,
                    "exit_signal": proc_state.exit_signal,
                }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Alternate screen = full-screen TUI, not a line CLI: say so, since
    // every line-oriented heuristic above is meaningless there.
    if let Some((trace, _, _)) = decode_raw(session) {
        let alt_screen = trace
            .modes
            .iter()
            .rev()
            .find(|m| m.mode == "alt_screen")
            .map(|m| m.set)
            .unwrap_or(false);
        if alt_screen {
            findings.push(Finding {
                id: "SH-ALTSCREEN".into(),
                rule_id: None,
                severity: "info".into(),
                category: "shell_cli".into(),
                summary: "alternate screen is active — this is a full-screen TUI, not a line CLI; prompt/completion heuristics do not apply.".into(),
                evidence: vec![ev_other(
                    "alt_screen_active",
                    "DECSET 1049 timeline shows alt screen engaged",
                    json!({}),
                )],
                confidence: 0.95,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    findings
}
