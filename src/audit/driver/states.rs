//! State family: application state-machine traversal (item 29) and
//! error-path presentation audits (item 30).
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::{KeyCode, KeyEvent};
use crate::execution::{execute_act, CanonicalAction};
use crate::semantic;
use crate::session::state::Session;
use serde_json::json;

use super::shared::{ev_other, ev_other_empty};
use crate::audit::Finding;

/// Run the states audit (Wave G item 66): find disabled controls and probe
/// whether they can still be focused via Tab (a disabled-but-focusable
/// control breaks keyboard semantics). One frame of disabled-control
/// inventory plus a bounded focus probe — never a fake "walked all states".
pub fn states_audit(session: &mut Session, max_tabs: u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (_screen, sem, _tree, _report) = match session.observe_fused(50) {
        Ok(t) => t,
        Err(e) => {
            findings.push(Finding {
                id: "STATES-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "states".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "states_observe_failed",
                    "session.observe failed at states audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let disabled: Vec<&semantic::Control> = sem.controls.iter().filter(|c| !c.enabled).collect();
    let empty_like: Vec<&semantic::Control> = sem
        .controls
        .iter()
        .filter(|c| c.enabled && c.label.trim().is_empty())
        .collect();

    if !disabled.is_empty() {
        findings.push(Finding {
            id: "STATES-DISABLED".into(),
            rule_id: None,
            severity: "info".into(),
            category: "states".into(),
            summary: format!(
                "{} disabled control(s) on this screen: {}",
                disabled.len(),
                disabled
                    .iter()
                    .map(|c| c.label.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            evidence: vec![ev_other(
                "disabled_controls",
                "controls whose enabled state inference reports disabled (grayed/dimmed)",
                json!({
                    "disabled": disabled.iter().map(|c| json!({
                        "id": c.id,
                        "label": c.label,
                        "kind": format!("{:?}", c.kind),
                        "focusable": c.focusable,
                    })).collect::<Vec<_>>(),
                }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    if !empty_like.is_empty() {
        findings.push(Finding {
            id: "STATES-EMPTY-CONTROLS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "states".into(),
            summary: format!(
                "{} enabled control(s) carry an empty label — state may be unreadable to agents",
                empty_like.len()
            ),
            evidence: vec![ev_other(
                "empty_labeled_controls",
                "enabled controls with blank labels",
                json!({
                    "controls": empty_like.iter().map(|c| json!({
                        "id": c.id,
                        "kind": format!("{:?}", c.kind),
                        "bounds": c.bounds,
                    })).collect::<Vec<_>>(),
                }),
            )],
            confidence: 0.6,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Disabled-but-focusable probe: walk up to max_tabs tabs and check
    // whether focus ever lands on a disabled control.
    let disabled_ids: std::collections::HashSet<&str> =
        disabled.iter().map(|c| c.id.as_str()).collect();
    if !disabled.is_empty() {
        let mut focused_disabled: Vec<String> = Vec::new();
        for _ in 0..max_tabs.min(10) {
            let tx = match execute_act(
                session,
                &CanonicalAction::Key {
                    key: KeyEvent::new(KeyCode::Tab),
                },
                60,
                400,
                false,
            ) {
                Ok(t) => t,
                Err(_) => break,
            };
            // FUSED through the live session (review P0.4) so a native focus
            // self-report is never lost to bare re-inference.
            let sem_after = session.fuse_screen(tx.after());
            if let (Some(id), Some(_label)) = (
                sem_after.focus.control_id.as_deref(),
                sem_after.focus.control.as_deref(),
            ) {
                if disabled_ids.contains(id) {
                    focused_disabled.push(id.to_string());
                }
            }
        }
        if !focused_disabled.is_empty() {
            findings.push(Finding {
                id: "STATES-DISABLED-FOCUSABLE".into(),
                rule_id: None,
                severity: "error".into(),
                category: "states".into(),
                summary: format!(
                    "Tab reached {} disabled control(s): disabled controls must not take focus",
                    focused_disabled.len()
                ),
                evidence: vec![ev_other(
                    "disabled_focus_reached",
                    "focus landed on a control whose enabled inference says disabled",
                    json!({
                        "controls": focused_disabled,
                        "note": "enabled-state inference is heuristic; verify against the app before fixing",
                    }),
                )],
                confidence: 0.75,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    if findings.is_empty() {
        findings.push(Finding {
            id: "STATES-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "states".into(),
            summary: format!(
                "No disabled, empty-labeled, or unfocusable-state issues across {} control(s)",
                sem.controls.len()
            ),
            evidence: vec![ev_other(
                "states_ok",
                "no disabled/empty-label findings on this frame",
                json!({ "controls": sem.controls.len() }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    findings
}

/// Run the errors audit (Wave G item 66): crash resistance + on-screen
/// error scan. Two probes: (1) send a bounded burst of random safe keys —
/// the app must survive (no exit, no signal) and keep rendering; (2) scan
/// the final frame for error-shaped text (panic, tracebacks, "error:"-style
/// prefixes). The burst is SAFE-class keys only (arrows/tab/escape) so the
/// probe cannot destroy user data by construction.
pub fn errors_audit(session: &mut Session, burst: u32) -> Vec<Finding> {
    use crate::backend::KeyModifiers;

    let mut findings = Vec::new();
    let was_running = match session.observe(30) {
        Ok(s) => s.process.running,
        Err(e) => {
            findings.push(Finding {
                id: "ERR-AUDIT-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "errors".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "errors_observe_failed",
                    "session.observe failed at errors audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };

    // (1) Crash resistance: a bounded burst of navigation-only keys.
    // Wave 4 item 39: Escape is NOT in this set — it can cancel a form,
    // close a dialog, discard state, or abort a workflow, which makes it
    // a mutation, not a probe. Tab/arrows only.
    let safe_keys = [
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
    ];
    let mut seed = 0x5eed_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let burst = burst.min(30);
    let mut sent = 0u32;
    for _ in 0..burst {
        let code = safe_keys[(next() as usize) % safe_keys.len()];
        let key = if code == KeyCode::Tab && next() % 4 == 0 {
            KeyEvent::with_modifiers(KeyCode::Tab, KeyModifiers::SHIFT)
        } else {
            KeyEvent::new(code)
        };
        match execute_act(session, &CanonicalAction::Key { key }, 40, 300, false) {
            Ok(_) => sent += 1,
            Err(_) => break,
        }
    }

    let after = session.observe(80);
    let (still_running, exited_clean) = match &after {
        Ok(s) => (s.process.running, s.process.exit_code.is_some()),
        Err(_) => (false, false),
    };
    if was_running && !still_running {
        findings.push(Finding {
            id: "ERR-CRASH".into(),
            rule_id: None,
            severity: "error".into(),
            category: "errors".into(),
            summary: format!(
                "App exited during a {}-key safe burst (arrows/tab/escape only) — crash resistance failed",
                sent
            ),
            evidence: vec![ev_other(
                "crash_during_safe_burst",
                "process left running state during the safe-key burst",
                json!({
                    "keys_sent": sent,
                    "burst_requested": burst,
                    "key_classes": "tab/shift+tab/arrows (escape excluded: it can mutate state)",
                    "exit_state": after.as_ref().ok().map(|s| &s.process),
                }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
        // Skip the error scan on a dead screen; the crash IS the finding.
        return findings;
    }
    let _ = exited_clean;

    // (2) On-screen error scan of the final frame.
    if let Ok(s) = after {
        let joined = s.viewport_text.join("\n");
        let lowered = joined.to_lowercase();
        // Wave 4 item 39: marker strength is tiered. Strong markers
        // (panic/traceback/segfault) are app-failure evidence on their
        // own. Weak markers ("error:", "fatal:", "exception:") also match
        // log viewers, docs, and compiler-output panels, so alone they are
        // a low-confidence hint — never an `error`-severity verdict.
        let strong_markers = [
            "panicked at",
            "traceback (most recent call last)",
            "unhandled exception",
            "segmentation fault",
        ];
        let weak_markers = ["error:", "fatal:", "exception:"];
        let strong: Vec<&str> = strong_markers
            .iter()
            .filter(|m| lowered.contains(*m))
            .cloned()
            .collect();
        let weak: Vec<&str> = weak_markers
            .iter()
            .filter(|m| lowered.contains(*m))
            .cloned()
            .collect();
        if !strong.is_empty() {
            // Pull the offending lines as evidence (bounded).
            let lines: Vec<String> = s
                .viewport_text
                .iter()
                .filter(|l| {
                    let ll = l.to_lowercase();
                    strong.iter().any(|m| ll.contains(m)) || weak.iter().any(|m| ll.contains(m))
                })
                .take(5)
                .cloned()
                .collect();
            findings.push(Finding {
                id: "ERR-ON-SCREEN".into(),
                rule_id: None,
                severity: "error".into(),
                category: "errors".into(),
                summary: format!(
                    "App-failure text visible on screen: strong marker(s) found ({})",
                    strong.join(", ")
                ),
                evidence: vec![ev_other(
                    "error_text_on_screen",
                    "viewport lines matching strong app-failure markers",
                    json!({
                        "strong_markers": strong,
                        "weak_markers": weak,
                        "lines": lines,
                        "structure_hash": s.structure_hash,
                    }),
                )],
                confidence: 0.9,
                reproduction: None,
                source_refs: Vec::new(),
            });
        } else if !weak.is_empty() {
            findings.push(Finding {
                id: "ERR-TEXT-HINT".into(),
                rule_id: None,
                severity: "info".into(),
                category: "errors".into(),
                summary: format!(
                    "error-shaped text visible ({}) with NO strong app-failure marker — could be a log viewer, docs, or compiler output; verify against process state before treating it as a crash.",
                    weak.join(", ")
                ),
                evidence: vec![ev_other(
                    "weak_error_markers",
                    "weak text markers without strong failure evidence",
                    json!({
                        "weak_markers": weak,
                        "process_running": s.process.running,
                        "structure_hash": s.structure_hash,
                        "note": "text markers alone are secondary evidence; unexpected exit/signal/panic is the primary signal",
                    }),
                )],
                confidence: 0.4,
                reproduction: None,
                source_refs: Vec::new(),
            });
        } else {
            findings.push(Finding {
                id: "ERR-OK".into(),
                rule_id: None,
                severity: "info".into(),
                category: "errors".into(),
                summary: format!(
                    "Survived {} safe keys with no exit and no error text on screen",
                    sent
                ),
                evidence: vec![ev_other(
                    "crash_resistance_ok",
                    "no exit, no signal, no error markers after the burst",
                    json!({
                        "keys_sent": sent,
                        "structure_hash": s.structure_hash,
                    }),
                )],
                confidence: 0.9,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }
    findings
}
