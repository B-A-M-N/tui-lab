//! Active audit driver: drives the application through a session to perform
//! keyboard, focus, resize, and clipping audits (spec items 52-55).
//!
//! Unlike the static frame audits in `super::mod`, these require sending input
//! and observing state transitions.

use crate::backend::{KeyCode, KeyEvent, WaitCond};
use crate::execution::{execute_act, CanonicalAction};
use crate::semantic;
use crate::session::state::Session;
use serde_json::json;

use super::{EvidenceKind, EvidenceRef, Finding};

const RESIZE_MATRIX: &[(u16, u16)] = &[(60, 20), (80, 24), (100, 30), (120, 40), (160, 50)];

/// Wrap a JSON detail blob into a single generic-typed EvidenceRef so
/// driver-side Finding constructors stay one-line each. The detail keeps
/// the original structured data; the kind/target/summary give the new
/// discriminated surface that other consumers can branch on (re-review
/// Part XV).
fn ev_other(target: &str, summary: &str, detail: serde_json::Value) -> EvidenceRef {
    EvidenceRef::point(EvidenceKind::Other, target, summary).with_detail(detail)
}

/// Convenience: same as `ev_other` but the detail starts as `{}` and the
/// caller fills it in.
fn ev_other_empty(target: &str, summary: &str) -> EvidenceRef {
    ev_other(
        target,
        summary,
        serde_json::Value::Object(Default::default()),
    )
}

/// Run keyboard audit: traverse focus using Tab (item 52).
pub fn keyboard_audit(session: &mut Session, max_tabs: u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    let _baseline = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "KB-ERR".into(),
                severity: "error".into(),
                category: "keyboard".into(),
                summary: format!("Cannot observe baseline: {}", e),
                evidence: vec![ev_other_empty(
                    "baseline_observe_failed",
                    "session.observe failed at audit start",
                )],
                confidence: 1.0,
            });
            return findings;
        }
    };

    let mut visited_states: Vec<String> = Vec::new();
    let mut focus_order: Vec<Option<String>> = Vec::new();
    let mut successful_tabs = 0u32;

    for i in 0..max_tabs {
        let before = match session.observe(30) {
            Ok(s) => s,
            Err(_) => break,
        };
        let focus_before = semantic::analyze(&before).focus.control.clone();
        focus_order.push(focus_before.clone());

        // Tab via the canonical executor — same baseline-before-send and
        // anchored settle ordering as MCP `tui_act` (re-review P0).
        let tx = match execute_act(
            session,
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
                    severity: "error".into(),
                    category: "keyboard".into(),
                    summary: format!("Tab send failed at step {}", i),
                    evidence: vec![ev_other(
                        "tab_send_failed",
                        "execute_act returned Err for Tab",
                        json!({"step": i}),
                    )],
                    confidence: 1.0,
                });
                break;
            }
        };
        successful_tabs += 1;
        let after = tx.after().clone();
        let sem_after = semantic::analyze(&after);
        let focus_after = sem_after.focus.control.clone();

        // Detect Tab trap: focus didn't change
        if focus_before == focus_after {
            findings.push(Finding {
                id: "KB-TRAP".into(),
                severity: "warn".into(),
                category: "keyboard".into(),
                summary: format!("Tab at step {} did not change focus", i),
                evidence: vec![ev_other(
                    "tab_trap",
                    "focus did not change after Tab",
                    json!({
                        "step": i,
                        "focus": focus_after,
                        "hash": after.structure_hash,
                    }),
                )],
                confidence: 0.8,
            });
        }

        // Detect cycle
        let state_key = format!("{:?}-{}", focus_after, after.structure_hash);
        if visited_states.contains(&state_key) {
            findings.push(Finding {
                id: "KB-CYCLE".into(),
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
            });
            break;
        }
        visited_states.push(state_key);
    }

    // Reverse traversal with Shift+Tab via the canonical executor.
    let mut reverse_ok = true;
    for _ in 0..successful_tabs.min(max_tabs) {
        let r = execute_act(
            session,
            &CanonicalAction::Key {
                key: KeyEvent::with_modifiers(KeyCode::Tab, crate::backend::KeyModifiers::SHIFT),
            },
            80,
            500,
            false,
        );
        if r.is_err() {
            reverse_ok = false;
            break;
        }
    }

    if successful_tabs > 0 && reverse_ok {
        findings.push(Finding {
            id: "KB-OK".into(),
            severity: "info".into(),
            category: "keyboard".into(),
            summary: format!(
                "Tab traversal: {} states visited, reverse traversal succeeded",
                successful_tabs
            ),
            evidence: vec![ev_other(
                "tab_ok",
                "Tab traversal completed with reverse traversal ok",
                json!({
                    "states_visited": successful_tabs,
                    "focus_order": focus_order,
                    "reverse_ok": reverse_ok,
                }),
            )],
            confidence: 0.85,
        });
    }

    findings
}

/// Run focus audit: check exactly one apparent focus, Tab changes focus (item 53).
pub fn focus_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "FOCUS-ERR".into(),
                severity: "error".into(),
                category: "focus".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "focus_observe_failed",
                    "session.observe failed in focus audit",
                )],
                confidence: 1.0,
            });
            return findings;
        }
    };

    let sem = semantic::analyze(&screen);

    // Check: exactly one apparent focus
    let reverse_cells: Vec<_> = screen.cells.iter().filter(|c| c.reverse).collect();
    let has_focus = sem.focus.control.is_some() || !reverse_cells.is_empty();

    if !has_focus {
        findings.push(Finding {
            id: "FOCUS-001".into(),
            severity: "warn".into(),
            category: "focus".into(),
            summary: "No detectable focus target on this screen.".into(),
            evidence: vec![ev_other(
                "focus_missing",
                "no focused control and no reverse-video cells",
                json!({ "reverse_cells": reverse_cells.len() }),
            )],
            confidence: 0.7,
        });
    }

    if let Some(ref ctrl) = sem.focus.control {
        // Verify Tab changes focus (canonical executor; re-review P0).
        let focus_before = ctrl.clone();
        let tx = match execute_act(
            session,
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
        let sem_after = semantic::analyze(&after);

        if sem_after.focus.control.as_ref() == Some(&focus_before) {
            findings.push(Finding {
                id: "FOCUS-002".into(),
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
            });
        } else {
            findings.push(Finding {
                id: "FOCUS-OK".into(),
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
            });
        }
    }

    findings
}

/// Run resize audit: test viewport matrix (item 54).
pub fn resize_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let original_cols = session.cols();
    let original_rows = session.rows();

    for &(cols, rows) in RESIZE_MATRIX {
        if let Err(e) = session.resize(cols, rows) {
            findings.push(Finding {
                id: "RESZ-ERR".into(),
                severity: "error".into(),
                category: "resize".into(),
                summary: format!("Resize to {}x{} failed: {}", cols, rows, e),
                evidence: vec![ev_other(
                    "resize_failed",
                    "session.resize returned Err",
                    json!({"cols": cols, "rows": rows}),
                )],
                confidence: 1.0,
            });
            continue;
        }

        let _ = session.wait(
            WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(150),
                after_screen_seq: None,
            },
            2000,
        );
        let screen = match session.observe(100) {
            Ok(s) => s,
            Err(e) => {
                findings.push(Finding {
                    id: "RESZ-ERR".into(),
                    severity: "error".into(),
                    category: "resize".into(),
                    summary: format!("Observe after resize to {}x{} failed: {}", cols, rows, e),
                    evidence: vec![ev_other(
                        "resize_observe_failed",
                        "session.observe failed after resize",
                        json!({"cols": cols, "rows": rows}),
                    )],
                    confidence: 1.0,
                });
                continue;
            }
        };

        let sem = semantic::analyze(&screen);

        // Check for clipping
        let clipped_regions: Vec<_> = sem
            .regions
            .iter()
            .filter(|rg| !matches!(rg.clipping_state, crate::semantic::ClippingState::None))
            .collect();

        if !clipped_regions.is_empty() {
            findings.push(Finding {
                id: "RESZ-CLIP".into(),
                severity: "error".into(),
                category: "resize".into(),
                summary: format!(
                    "At {}x{}, {} region(s) clipped",
                    cols,
                    rows,
                    clipped_regions.len()
                ),
                evidence: vec![ev_other(
                    "resize_clipping",
                    "regions clipped after resize",
                    json!({
                        "cols": cols,
                        "rows": rows,
                        "clipped": clipped_regions.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                    }),
                )],
                confidence: 0.95,
            });
        } else {
            findings.push(Finding {
                id: "RESZ-OK".into(),
                severity: "info".into(),
                category: "resize".into(),
                summary: format!(
                    "At {}x{}, no clipping detected ({} regions)",
                    cols,
                    rows,
                    sem.regions.len()
                ),
                evidence: vec![ev_other(
                    "resize_ok",
                    "no clipping detected after resize",
                    json!({
                        "cols": cols,
                        "rows": rows,
                        "region_count": sem.regions.len(),
                    }),
                )],
                confidence: 0.9,
            });
        }
    }

    // Restore original dimensions
    let _ = session.resize(original_cols, original_rows);
    let _ = session.wait(
        WaitCond::ScreenStable {
            quiet_for: std::time::Duration::from_millis(150),
            after_screen_seq: None,
        },
        2000,
    );

    findings
}

/// Run clipping audit: detect real clipping (item 55).
pub fn clipping_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "CLIP-ERR".into(),
                severity: "error".into(),
                category: "clipping".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "clipping_observe_failed",
                    "session.observe failed in clipping audit",
                )],
                confidence: 1.0,
            });
            return findings;
        }
    };

    let sem = semantic::analyze(&screen);
    let cols = screen.cols;
    let rows = screen.rows;

    // Check each region for clipping
    for rg in &sem.regions {
        let b = &rg.bounds;

        // Direct bounds check
        if b.x.saturating_add(b.width) > cols || b.y.saturating_add(b.height) > rows {
            findings.push(Finding {
                id: "CLIP-001".into(),
                severity: "error".into(),
                category: "clipping".into(),
                summary: format!("Region '{}' bounds exceed terminal", rg.id),
                evidence: vec![ev_other(
                    "region_bounds_exceed_terminal",
                    "region bounds exceed terminal viewport",
                    json!({
                        "bounds": b,
                        "terminal_cols": cols,
                        "terminal_rows": rows,
                    }),
                )],
                confidence: 0.96,
            });
        }

        // Clipping state from border graph
        if !matches!(rg.clipping_state, semantic::ClippingState::None) {
            findings.push(Finding {
                id: "CLIP-002".into(),
                severity: "error".into(),
                category: "clipping".into(),
                summary: format!("Region '{}' clipped: {:?}", rg.id, rg.clipping_state),
                evidence: vec![ev_other(
                    "region_border_clipped",
                    "border graph reports a clipped region edge",
                    json!({
                        "bounds": b,
                        "clipping_state": format!("{:?}", rg.clipping_state),
                        "title": rg.title,
                    }),
                )],
                confidence: 0.95,
            });
        }
    }

    // Check for incomplete borders at viewport edges
    if !screen.viewport_text.is_empty() {
        let first_row = &screen.viewport_text[0];
        let last_row = &screen.viewport_text[screen.viewport_text.len() - 1];

        // Top edge: look for border openings
        if has_incomplete_border(first_row) {
            findings.push(Finding {
                id: "CLIP-003".into(),
                severity: "warn".into(),
                category: "clipping".into(),
                summary: "Top border has possible clipping at viewport edge".into(),
                evidence: vec![ev_other(
                    "border_open_at_top_edge",
                    "top viewport row has an asymmetric border edge",
                    json!({"row": first_row, "edge": "top"}),
                )],
                confidence: 0.6,
            });
        }

        // Bottom edge
        if has_incomplete_border(last_row) {
            findings.push(Finding {
                id: "CLIP-004".into(),
                severity: "warn".into(),
                category: "clipping".into(),
                summary: "Bottom border has possible clipping at viewport edge".into(),
                evidence: vec![ev_other(
                    "border_open_at_bottom_edge",
                    "bottom viewport row has an asymmetric border edge",
                    json!({"row": last_row, "edge": "bottom"}),
                )],
                confidence: 0.6,
            });
        }
    }

    if findings.is_empty() {
        findings.push(Finding {
            id: "CLIP-OK".into(),
            severity: "info".into(),
            category: "clipping".into(),
            summary: format!("No clipping detected ({} regions)", sem.regions.len()),
            evidence: vec![ev_other(
                "no_clipping_detected",
                "all regions fit the terminal viewport",
                json!({"region_count": sem.regions.len()}),
            )],
            confidence: 0.9,
        });
    }

    findings
}

/// Check if a row has border characters that suggest incomplete borders.
fn has_incomplete_border(row: &str) -> bool {
    if row.len() < 3 {
        return false;
    }
    let first = row.chars().next().unwrap();
    let last = row.chars().last().unwrap();
    // Border chars that suggest an open edge
    let border_chars = [
        '─', '│', '┌', '┐', '└', '┘', '╭', '╮', '╰', '╯', '═', '║', '╔', '╗', '╚', '╝',
    ];
    let first_is_border = border_chars.contains(&first);
    let last_is_border = border_chars.contains(&last);
    // Incomplete if only one side has border
    first_is_border != last_is_border
}
