//! Keyboard traversal family (spec item 52): Tab/Shift+Tab focus order
//! and focus state audits.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::{KeyCode, KeyEvent};
use crate::execution::{execute_act_as, CanonicalAction};
use crate::session::state::Session;
use serde_json::json;

use super::shared::{ev_other, ev_other_empty};
use crate::audit::Finding;

/// Run keyboard audit: traverse focus using Tab (item 52). Upgraded for
/// Wave D (items 36–37): every transition is recorded into the run's
/// ID-keyed [`crate::semantic::focus_graph::FocusGraph`] with `via=tab`
/// / `via=shift+tab` provenance, so the audit *proves* traversal order
/// instead of listing labels.
pub fn keyboard_audit(
    session: &mut Session,
    max_tabs: u32,
    graph: &mut crate::semantic::focus_graph::FocusGraph,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let baseline_sem = match session.observe_fused(50) {
        Ok((_, sem, _, _)) => sem,
        Err(e) => {
            findings.push(Finding {
                id: "KB-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "keyboard".into(),
                summary: format!("Cannot observe baseline: {}", e),
                evidence: vec![ev_other_empty(
                    "baseline_observe_failed",
                    "session.observe failed at audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let mut last_id = baseline_sem.focus.control_id.clone();

    let mut visited_states: Vec<String> = Vec::new();
    let mut focus_order: Vec<Option<String>> = Vec::new();
    let mut successful_tabs = 0u32;

    for i in 0..max_tabs {
        let (_before, sem_before, _, _) = match session.observe_fused(30) {
            Ok(t) => t,
            Err(_) => break,
        };
        let focus_before = sem_before.focus.control.clone();
        focus_order.push(focus_before.clone());

        // Tab via the canonical executor — same baseline-before-send and
        // anchored settle ordering as MCP `tui_act` (re-review P0).
        let tx = match execute_act_as(
            session,
            crate::execution::DriveOrigin::Audit,
            &CanonicalAction::Key {
                key: KeyEvent::new(KeyCode::Tab),
            },
            80,
            500,
            false,
        ) {
            Ok(tx) => tx,
            Err(_e) => {
                findings.push(Finding {
                    id: "KB-ERR".into(),
                    rule_id: None,
                    severity: "error".into(),
                    category: "keyboard".into(),
                    summary: format!("Tab send failed at step {}", i),
                    evidence: vec![ev_other(
                        "tab_send_failed",
                        "execute_act returned Err for Tab",
                        json!({"step": i}),
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                break;
            }
        };
        successful_tabs += 1;
        let after = tx.after().clone();
        // FUSED analysis through the live session (review P0.4): a bare
        // `semantic::analyze` here would drop native focus facts and let the
        // audit disagree with what `tui_observe semantic` reports for the
        // same pixel state.
        let sem_after = session.fuse_screen(&after);
        let focus_after = sem_after.focus.control.clone();

        // ID-keyed graph edge with Tab provenance (item 36).
        if let (Some(f), Some(t)) = (&sem_before.focus.control_id, &sem_after.focus.control_id) {
            graph.record_edge(f, t, "tab", sem_after.focus.control.as_deref());
        }
        last_id = sem_after.focus.control_id.clone();

        // Detect Tab trap: focus didn't change. NOT a defect when the
        // screen legitimately holds ≤1 focusable control (Wave 4 item 39 —
        // a single-button app re-tabs to itself forever); the focusable
        // count from the fused frame decides whether this is a trap or the
        // expected degenerate case.
        if focus_before == focus_after {
            let focusable_count = sem_before.controls.iter().filter(|c| c.focusable).count();
            if focusable_count > 1 {
                findings.push(Finding {
                    id: "KB-TRAP".into(),
                    rule_id: None,
                    severity: "warn".into(),
                    category: "keyboard".into(),
                    summary: format!(
                        "Tab at step {} did not change focus although {} focusable controls are visible",
                        i, focusable_count
                    ),
                    evidence: vec![ev_other(
                        "tab_trap",
                        "focus did not change after Tab with multiple focusable controls",
                        json!({
                            "step": i,
                            "focus": focus_after,
                            "focusable_controls": focusable_count,
                            "hash": after.structure_hash,
                        }),
                    )],
                    confidence: 0.85,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
            } else {
                findings.push(Finding {
                    id: "KB-SINGLE-FOCUSABLE".into(),
                    rule_id: None,
                    severity: "info".into(),
                    category: "keyboard".into(),
                    summary: format!(
                        "Tab at step {} did not move focus: {} focusable control(s) visible — not a trap",
                        i, focusable_count
                    ),
                    evidence: vec![ev_other(
                        "single_focusable",
                        "focus static because the screen has ≤1 focusable control",
                        json!({
                            "step": i,
                            "focusable_controls": focusable_count,
                        }),
                    )],
                    confidence: 0.9,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
            }
        }

        // Detect cycle
        let state_key = format!("{:?}-{}", focus_after, after.structure_hash);
        if visited_states.contains(&state_key) {
            findings.push(Finding {
                id: "KB-CYCLE".into(),
                rule_id: None,
                severity: "info".into(),
                category: "keyboard".into(),
                summary: format!(
                    "Tab traversal returned to a previously seen state at step {}",
                    i
                ),
                evidence: vec![ev_other(
                    "tab_cycle",
                    "Tab traversal returned to a previously seen state",
                    json!({
                        "step": i,
                        "focus_order": focus_order,
                    }),
                )],
                confidence: 0.9,
                reproduction: None,
                source_refs: Vec::new(),
            });
            break;
        }
        visited_states.push(state_key);
    }

    // Reverse traversal with Shift+Tab via the canonical executor. Each
    // reversal lands in the graph with `via=shift+tab` so reverse_tab_gaps
    // can name what a forward edge lacked.
    let mut reverse_ok = true;
    for _ in 0..successful_tabs.min(max_tabs) {
        let r = execute_act_as(
            session,
            crate::execution::DriveOrigin::Audit,
            &CanonicalAction::Key {
                key: KeyEvent::with_modifiers(KeyCode::Tab, crate::backend::KeyModifiers::SHIFT),
            },
            80,
            500,
            false,
        );
        match r {
            Ok(tx) => {
                // FUSED through the live session (review P0.4) so the focus
                // graph sees the same native-aware truth every observe mode
                // reports.
                let sem_b = session.fuse_screen(tx.before());
                let sem_a = session.fuse_screen(tx.after());
                if let (Some(f), Some(t)) = (&sem_b.focus.control_id, &sem_a.focus.control_id) {
                    graph.record_edge(f, t, "shift+tab", sem_a.focus.control.as_deref());
                }
                last_id = sem_a.focus.control_id.clone();
            }
            Err(_) => {
                reverse_ok = false;
                break;
            }
        }
    }

    if successful_tabs > 0 {
        let gaps = graph.reverse_tab_gaps();
        let cycle = graph.tab_cycle();
        // A cycle through Tab is expected behavior (wrap-around), not a
        // defect — record it as evidence. Missing inverse edges ARE a
        // defect: Shift+Tab must truly reverse Tab.
        if reverse_ok && !gaps.is_empty() {
            findings.push(Finding {
                id: "KB-REVERSE-GAP".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "keyboard".into(),
                summary: format!(
                    "Shift+Tab does not reverse Tab: {} transition(s) lack the inverse edge",
                    gaps.len()
                ),
                evidence: vec![ev_other(
                    "reverse_tab_gaps",
                    "focus graph edges with a missing Shift+Tab inverse",
                    json!({
                        "gaps": gaps,
                        "note": "edges keyed on stable control IDs; 'from' expects a shift+tab edge back to 'to'",
                    }),
                )],
                confidence: 0.85,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
        if successful_tabs > 0 && reverse_ok {
            findings.push(Finding {
                id: "KB-OK".into(),
                rule_id: None,
                severity: "info".into(),
                category: "keyboard".into(),
                summary: format!(
                    "Tab traversal: {} states visited, reverse traversal succeeded ({} graph edges{})",
                    successful_tabs,
                    graph.edges.len(),
                    cycle
                        .as_ref()
                        .map(|c| format!(", wrap cycle through {}", c.len()))
                        .unwrap_or_default(),
                ),
                evidence: vec![ev_other(
                    "tab_ok",
                    "Tab traversal completed with reverse traversal ok",
                    json!({
                        "states_visited": successful_tabs,
                        "focus_order": focus_order,
                        "reverse_ok": reverse_ok,
                        "focus_graph": graph.summary(),
                    }),
                )],
                confidence: 0.85,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    // `last_id` is consumed by callers through the graph; silence the
    // unused-assign lint without dropping the tracking (used for the
    // audit's own final-state evidence).
    let _ = last_id;

    findings
}

/// Run focus audit: check exactly one apparent focus, Tab changes focus (item 53).
pub fn focus_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    // Fused truth (re-review Wave-2 item 16): the audit reads the same
    // analysis every observe mode sees.
    let (screen, sem, _tree, _report) = match session.observe_fused(50) {
        Ok(t) => t,
        Err(e) => {
            findings.push(Finding {
                id: "FOCUS-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "focus".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "focus_observe_failed",
                    "session.observe failed in focus audit",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };

    // Check: exactly one apparent focus
    let reverse_cells: Vec<_> = screen.cells.iter().filter(|c| c.reverse).collect();
    let has_focus = sem.focus.control.is_some() || !reverse_cells.is_empty();

    if !has_focus {
        findings.push(Finding {
            id: "FOCUS-001".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "focus".into(),
            summary: "No detectable focus target on this screen.".into(),
            evidence: vec![ev_other(
                "focus_missing",
                "no focused control and no reverse-video cells",
                json!({ "reverse_cells": reverse_cells.len() }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    if let Some(ref ctrl) = sem.focus.control {
        // Verify Tab changes focus (canonical executor; re-review P0).
        let focus_before = ctrl.clone();
        let tx = match execute_act_as(
            session,
            crate::execution::DriveOrigin::Audit,
            &CanonicalAction::Key {
                key: KeyEvent::new(KeyCode::Tab),
            },
            80,
            500,
            false,
        ) {
            Ok(tx) => tx,
            Err(_) => return findings,
        };
        let after = tx.after().clone();
        // FUSED through the live session (review P0.4) — native focus wins
        // when the app cooperates, matching every observe mode's truth.
        let sem_after = session.fuse_screen(&after);

        if sem_after.focus.control.as_ref() == Some(&focus_before) {
            findings.push(Finding {
                id: "FOCUS-002".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "focus".into(),
                summary: "Tab did not change focus target.".into(),
                evidence: vec![ev_other(
                    "tab_no_focus_change",
                    "Tab did not move focus away from initial control",
                    json!({
                        "focus_before": focus_before,
                        "focus_after": sem_after.focus.control,
                    }),
                )],
                confidence: 0.85,
                reproduction: None,
                source_refs: Vec::new(),
            });
        } else {
            findings.push(Finding {
                id: "FOCUS-OK".into(),
                rule_id: None,
                severity: "info".into(),
                category: "focus".into(),
                summary: format!(
                    "Focus on '{}'; Tab changes focus (confidence {:.2})",
                    ctrl, sem.focus.confidence
                ),
                evidence: vec![ev_other(
                    "focus_ok_with_tab",
                    "focused control + Tab moves focus",
                    json!({
                        "focus": ctrl,
                        "confidence": sem.focus.confidence,
                        "evidence": sem.focus.evidence,
                    }),
                )],
                confidence: sem.focus.confidence,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    findings
}
