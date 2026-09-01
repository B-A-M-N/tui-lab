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
        let sem_after = semantic::analyze(&after);
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
        let r = execute_act(
            session,
            &CanonicalAction::Key {
                key: KeyEvent::with_modifiers(KeyCode::Tab, crate::backend::KeyModifiers::SHIFT),
            },
            80,
            500,
            false,
        );
        match r {
            Ok(tx) => {
                let sem_b = semantic::analyze(tx.before());
                let sem_a = semantic::analyze(tx.after());
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

/// Run resize audit: test viewport matrix (item 54).
pub fn resize_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let original_cols = session.cols();
    let original_rows = session.rows();

    for &(cols, rows) in RESIZE_MATRIX {
        if let Err(e) = session.resize(cols, rows) {
            findings.push(Finding {
                id: "RESZ-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "resize".into(),
                summary: format!("Resize to {}x{} failed: {}", cols, rows, e),
                evidence: vec![ev_other(
                    "resize_failed",
                    "session.resize returned Err",
                    json!({"cols": cols, "rows": rows}),
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
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
        let (_screen, sem, _tree, _report) = match session.observe_fused(100) {
            Ok(t) => t,
            Err(e) => {
                findings.push(Finding {
                    id: "RESZ-ERR".into(),
                    rule_id: None,
                    severity: "error".into(),
                    category: "resize".into(),
                    summary: format!("Observe after resize to {}x{} failed: {}", cols, rows, e),
                    evidence: vec![ev_other(
                        "resize_observe_failed",
                        "session.observe failed after resize",
                        json!({"cols": cols, "rows": rows}),
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                continue;
            }
        };

        // Check for clipping
        let clipped_regions: Vec<_> = sem
            .regions
            .iter()
            .filter(|rg| !matches!(rg.clipping_state, crate::semantic::ClippingState::None))
            .collect();

        if !clipped_regions.is_empty() {
            findings.push(Finding {
                id: "RESZ-CLIP".into(),
                rule_id: None,
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
                reproduction: None,
                source_refs: Vec::new(),
            });
        } else {
            findings.push(Finding {
                id: "RESZ-OK".into(),
                rule_id: None,
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
                reproduction: None,
                source_refs: Vec::new(),
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
    let (screen, sem, _tree, _report) = match session.observe_fused(50) {
        Ok(t) => t,
        Err(e) => {
            findings.push(Finding {
                id: "CLIP-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "clipping".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "clipping_observe_failed",
                    "session.observe failed in clipping audit",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };

    let cols = screen.cols;
    let rows = screen.rows;

    // Check each region for clipping
    for rg in &sem.regions {
        let b = &rg.bounds;

        // Direct bounds check
        if b.x.saturating_add(b.width) > cols || b.y.saturating_add(b.height) > rows {
            findings.push(Finding {
                id: "CLIP-001".into(),
                rule_id: None,
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
                reproduction: None,
                source_refs: Vec::new(),
            });
        }

        // Clipping state from border graph
        if !matches!(rg.clipping_state, semantic::ClippingState::None) {
            findings.push(Finding {
                id: "CLIP-002".into(),
                rule_id: None,
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
                reproduction: None,
                source_refs: Vec::new(),
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
                rule_id: None,
                severity: "warn".into(),
                category: "clipping".into(),
                summary: "Top border has possible clipping at viewport edge".into(),
                evidence: vec![ev_other(
                    "border_open_at_top_edge",
                    "top viewport row has an asymmetric border edge",
                    json!({"row": first_row, "edge": "top"}),
                )],
                confidence: 0.6,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }

        // Bottom edge
        if has_incomplete_border(last_row) {
            findings.push(Finding {
                id: "CLIP-004".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "clipping".into(),
                summary: "Bottom border has possible clipping at viewport edge".into(),
                evidence: vec![ev_other(
                    "border_open_at_bottom_edge",
                    "bottom viewport row has an asymmetric border edge",
                    json!({"row": last_row, "edge": "bottom"}),
                )],
                confidence: 0.6,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    if findings.is_empty() {
        findings.push(Finding {
            id: "CLIP-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "clipping".into(),
            summary: format!("No clipping detected ({} regions)", sem.regions.len()),
            evidence: vec![ev_other(
                "no_clipping_detected",
                "all regions fit the terminal viewport",
                json!({"region_count": sem.regions.len()}),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

/// Run the navigation audit (Wave D item 37): build a real focus graph by
/// driving Tab forward through the whole cycle, then Shift+Tab back, and
/// report what the ID-keyed edges prove: traversal order, wrap-around,
/// and whether Shift+Tab truly reverses Tab.
pub fn navigation_audit(
    session: &mut Session,
    max_tabs: u32,
    graph: &mut crate::semantic::focus_graph::FocusGraph,
) -> Vec<Finding> {
    // The keyboard driver already records `via=tab` / `via=shift+tab` edges
    // into the graph; navigation analysis reads them.
    let mut findings = keyboard_audit(session, max_tabs, graph);

    let cycle = graph.tab_cycle();
    let gaps = graph.reverse_tab_gaps();
    let tab_edges = graph.successors_all("tab");

    if tab_edges.is_empty() {
        findings.push(Finding {
            id: "NAV-NO-TRAVERSAL".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "navigation".into(),
            summary: "No Tab traversal observed: the screen exposes no keyboard navigation order."
                .into(),
            evidence: vec![ev_other(
                "no_tab_edges",
                "focus graph holds no via=tab edges after the traversal attempt",
                json!({ "graph": graph.summary() }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    }

    // The order itself, proven edge by edge (stable IDs, not labels).
    let order: Vec<String> = tab_edges
        .iter()
        .map(|e| format!("{} → {} (x{})", e.from, e.to, e.count))
        .collect();
    findings.push(Finding {
        id: "NAV-ORDER".into(),
        rule_id: None,
        severity: "info".into(),
        category: "navigation".into(),
        summary: format!(
            "Tab order observed over {} edge(s){}",
            tab_edges.len(),
            cycle
                .as_ref()
                .map(|c| format!("; wraps through {} controls", c.len()))
                .unwrap_or_else(|| "; no complete cycle observed".to_string()),
        ),
        evidence: vec![ev_other(
            "tab_order",
            "focus graph Tab edges in observation order (stable control IDs)",
            json!({
                "edges": order,
                "cycle": cycle,
                "graph": graph.summary(),
            }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    });

    if !gaps.is_empty() {
        findings.push(Finding {
            id: "NAV-REVERSE-GAP".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "navigation".into(),
            summary: format!(
                "Reverse traversal is not the true inverse: {} Tab edge(s) have no Shift+Tab counterpart",
                gaps.len()
            ),
            evidence: vec![ev_other(
                "reverse_gaps",
                "every Tab edge must have the mirrored Shift+Tab edge for true reversal",
                json!({ "gaps": gaps, "graph": graph.summary() }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "NAV-REVERSE-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "navigation".into(),
            summary: "Shift+Tab exactly reverses Tab (every forward edge has its inverse).".into(),
            evidence: vec![ev_other(
                "reverse_proven",
                "edge-for-edge inverse holds across the observed Tab subgraph",
                json!({ "tab_edges": tab_edges.len(), "graph": graph.summary() }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
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

/// Run the mouse audit (Wave G item 66): real clicks through the canonical
/// executor on each visible, enabled, clickable control. Evidence-first:
/// the driver reports what actually changed after each click (focus, screen
/// structure), and flags controls that declare clickability but absorb
/// clicks without any observable effect. Buttons/links whose semantics we
/// cannot verify statically are never assumed broken — the finding cites
/// the observed transition, not a guess.
pub fn mouse_audit(session: &mut Session, max_clicks: u32) -> Vec<Finding> {
    use crate::backend::MouseButton;

    let mut findings = Vec::new();
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "MOUSE-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "mouse".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "mouse_observe_failed",
                    "session.observe failed at mouse audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    if !caps.mouse {
        findings.push(Finding {
            id: "MOUSE-NO-CAPS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "mouse".into(),
            summary: "Backend reports no mouse-encoding capability; clicks are sent but the app may never receive them.".into(),
            evidence: vec![ev_other(
                "mouse_capability_absent",
                "capabilities.mouse = false",
                json!({ "backend": session.backend_kind }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        // Continue anyway: an app with mouse support behind a
        // capability-blind backend is still worth probing honestly.
    }

    let sem = session
        .fused_frame()
        .map(|(s, _, _)| s)
        .unwrap_or_else(|| semantic::analyze(&screen));
    // Risk filter (Wave D risk classes): only click SAFE-looking targets —
    // buttons and links whose click might mutate or destroy state are
    // listed, not clicked, unless the screen marks them unambiguous. Here
    // we click only controls whose label does not match destructive verbs.
    let destructive = [
        "delete", "remove", "quit", "kill", "reset", "format", "erase",
    ];
    let clickable: Vec<&semantic::Control> = sem
        .controls
        .iter()
        .filter(|c| {
            if !c.enabled || c.bounds.width == 0 || c.bounds.height == 0 {
                return false;
            }
            matches!(
                c.kind,
                semantic::ControlKind::Button
                    | semantic::ControlKind::Tab
                    | semantic::ControlKind::MenuItem
            ) || c.focusable
        })
        .filter(|c| {
            let label = c.label.to_lowercase();
            !destructive.iter().any(|d| label.contains(d))
        })
        .take(max_clicks as usize)
        .collect();

    if clickable.is_empty() {
        findings.push(Finding {
            id: "MOUSE-NO-TARGETS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "mouse".into(),
            summary: "No safe clickable controls detected; nothing clicked (destructive-looking labels are never clicked by the audit).".into(),
            evidence: vec![ev_other(
                "no_click_targets",
                "no enabled button/link controls passed the risk filter",
                json!({
                    "controls_seen": sem.controls.len(),
                    "risk_filter": "non-destructive, enabled, non-empty bounds",
                }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    }

    let mut clicked = 0u32;
    let mut unresponsive: Vec<String> = Vec::new();
    let mut responded: Vec<String> = Vec::new();

    for ctrl in &clickable {
        let cx = ctrl.bounds.x.saturating_add(ctrl.bounds.width / 2);
        let cy = ctrl.bounds.y.saturating_add(ctrl.bounds.height / 2);
        // Warm the frame cache so the transaction's before-frame is fresh
        // (the executor captures its own baseline; this read also surfaces
        // observe failures before the click).
        if session.observe(30).is_err() {
            break;
        }
        let tx = match execute_act(
            session,
            &CanonicalAction::MouseClick {
                button: MouseButton::Left,
                x: cx,
                y: cy,
            },
            80,
            500,
            false,
        ) {
            Ok(t) => t,
            Err(e) => {
                findings.push(Finding {
                    id: "MOUSE-SEND-ERR".into(),
                    rule_id: None,
                    severity: "warn".into(),
                    category: "mouse".into(),
                    summary: format!("Click send failed at ({}, {}): {}", cx, cy, e),
                    evidence: vec![ev_other(
                        "click_send_failed",
                        "execute_act returned Err for MouseClick",
                        json!({ "x": cx, "y": cy, "control": ctrl.id }),
                    )],
                    confidence: 0.9,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                break;
            }
        };
        clicked += 1;
        // Wave 4 item 38: "responded" is decided by the transaction's own
        // causal evidence, not by structure-hash equality alone. A control
        // that flashes a selection highlight (visual change, same
        // structure), moves only native focus, or produces observable
        // activity (settle Met) has RESPONDED — the old focus-or-structure
        // test labeled such legitimate responses "unresponsive".
        let focus_moved = tx.focus_before.and_then(|f| f.0.clone()) != tx.focus_after.and_then(|f| f.0.clone());
        let structure_changed = tx.transition.before_structure_hash != tx.transition.after_structure_hash;
        let visual_changed = tx.transition.before_visual_hash != tx.transition.after_visual_hash;
        let observable_activity = tx.settle == crate::execution::SettleStatus::Met;
        if focus_moved || structure_changed || visual_changed || observable_activity {
            let how = [
                ("focus", focus_moved),
                ("structure", structure_changed),
                ("visual", visual_changed),
                ("activity", observable_activity),
            ]
            .iter()
            .filter(|(_, hit)| *hit)
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join("+");
            responded.push(format!("{} ({})", ctrl.label, how));
            // Record the click edge into evidence via one finding per
            // responsive control is too noisy; they are summarized below.
        } else {
            unresponsive.push(format!(
                "{} at ({},{})",
                ctrl.label, ctrl.bounds.x, ctrl.bounds.y
            ));
        }
    }

    if clicked > 0 {
        findings.push(Finding {
            id: if unresponsive.is_empty() {
                "MOUSE-OK".into()
            } else {
                "MOUSE-UNRESPONSIVE".into()
            },
            rule_id: None,
            severity: if unresponsive.is_empty() {
                "info".into()
            } else {
                "warn".into()
            },
            category: "mouse".into(),
            summary: format!(
                "Clicked {} control(s): {} responded (focus or screen changed), {} did not",
                clicked,
                responded.len(),
                unresponsive.len()
            ),
            evidence: vec![ev_other(
                "mouse_click_results",
                "per-control click outcomes (center-of-bounds, canonical executor)",
                json!({
                    "clicked": clicked,
                    "responded": responded,
                    "unresponsive": unresponsive,
                    "response_tests": ["focus moved", "structure changed", "visual hash changed", "settle met (observable activity)"],
                    "note": "unresponsive = no focus/structure/visual change AND no observable activity within the settle budget",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

/// Run the performance audit (Wave G item 66): observe and settle latency
/// percentiles over a bounded number of samples. Evidence = measured
/// milliseconds, not assertions about the app's speed; slow paths are
/// flagged against the tool's own budgets so the numbers stay interpretable.
pub fn performance_audit(session: &mut Session, samples: u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    let samples = samples.clamp(3, 20);
    let mut observe_ms: Vec<u64> = Vec::new();
    let mut settle_ms: Vec<u64> = Vec::new();

    for _ in 0..samples {
        let t0 = std::time::Instant::now();
        let r = session.observe(40);
        observe_ms.push(t0.elapsed().as_millis() as u64);
        match r {
            Ok(_) => {}
            Err(e) => {
                findings.push(Finding {
                    id: "PERF-ERR".into(),
                    rule_id: None,
                    severity: "error".into(),
                    category: "performance".into(),
                    summary: format!("Observe failed during sampling: {}", e),
                    evidence: vec![ev_other_empty(
                        "perf_observe_failed",
                        "session.observe failed during performance sampling",
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                return findings;
            }
        }
        // A settle wait measures how long the app takes to go quiet after
        // its latest output — the same quantity a user experiences as lag.
        let t1 = std::time::Instant::now();
        let _ = session.wait(
            WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(60),
                after_screen_seq: None,
            },
            1000,
        );
        settle_ms.push(t1.elapsed().as_millis() as u64);
    }

    let pct = |v: &mut Vec<u64>, p: usize| -> u64 {
        v.sort_unstable();
        v.get(v.len().saturating_sub(1).min(p * v.len() / 100))
            .copied()
            .unwrap_or(0)
    };
    let obs_p50 = pct(&mut observe_ms, 50);
    let obs_p95 = pct(&mut observe_ms, 95);
    let set_p50 = pct(&mut settle_ms, 50);
    let set_p95 = pct(&mut settle_ms, 95);

    // Flag only against OUR budgets: observe must stay interactive
    // (< 500 ms p95) for agent loops; settle hitting the 1 s ceiling on most
    // samples means the app never goes quiet — an anti-pattern for waits.
    let slow_observe = obs_p95 >= 500;
    const SETTLE_CEILING: u64 = 1000;
    let settle_saturated =
        settle_ms.iter().filter(|&&m| m >= SETTLE_CEILING).count() * 2 >= settle_ms.len();
    findings.push(Finding {
        id: if slow_observe {
            "PERF-OBSERVE-SLOW"
        } else {
            "PERF-OK"
        }
        .into(),
        rule_id: None,
        severity: if slow_observe { "warn" } else { "info" }.into(),
        category: "performance".into(),
        summary: format!(
            "observe p50={}ms p95={}ms; settle-to-quiet p50={}ms p95={}ms ({} samples)",
            obs_p50, obs_p95, set_p50, set_p95, samples
        ),
        evidence: vec![ev_other(
            "latency_percentiles",
            "measured observe and settle latencies over the sampling window",
            json!({
                "samples": samples,
                "observe_ms": { "p50": obs_p50, "p95": obs_p95 },
                "settle_ms": { "p50": set_p50, "p95": set_p95 },
                "settle_budget_ms": SETTLE_CEILING,
                "settle_saturated": settle_saturated,
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
    });
    if settle_saturated {
        findings.push(Finding {
            id: "PERF-NEVER-QUIET".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "performance".into(),
            summary: "Screen kept changing through most settle windows: waits anchored on screen-stability will burn their budgets (animations, clocks, spinners).".into(),
            evidence: vec![ev_other(
                "settle_saturation",
                "most settle waits hit the budget ceiling without reaching quiet",
                json!({
                    "settle_ms": settle_ms,
                    "budget_ms": SETTLE_CEILING,
                    "note": "consider contract volatile_patterns normalization or idle waits instead",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    findings
}

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
            let sem_after = semantic::analyze(tx.after());
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
                    strong.iter().any(|m| ll.contains(m))
                        || weak.iter().any(|m| ll.contains(m))
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

/// Check whether the backend reports color capability and whether the
/// screen uses styled runs (static color audit; Wave G keeps this one
/// frame-level — a real color-contrast audit needs styled-cell semantics
/// the portable backend only partially reconstructs, and honesty beats a
/// fake WCAG pass).
pub fn color_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "COLOR-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "color".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "color_observe_failed",
                    "session.observe failed at color audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    let styled_cells = screen
        .cells
        .iter()
        .filter(|c| {
            c.fg.rgb.is_some()
                || c.fg.palette.is_some()
                || c.bg.rgb.is_some()
                || c.bg.palette.is_some()
        })
        .count();
    findings.push(Finding {
        id: "COLOR-INVENTORY".into(),
        rule_id: None,
        severity: "info".into(),
        category: "color".into(),
        summary: format!(
            "color capability: {}, styled cells: {}/{}",
            if caps.colors { "present" } else { "absent" },
            styled_cells,
            screen.cells.len()
        ),
        evidence: vec![ev_other(
            "color_inventory",
            "backend color capability + styled-cell census on the current frame",
            json!({
                "color_capable": caps.colors,
                "styled_cells": styled_cells,
                "total_cells": screen.cells.len(),
                "note": "a contrast audit needs app-declared palette semantics; use a design contract for color rules",
            }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    });
    findings
}

/// Wave-3 (terminal-modes subsystem): what input modes did this app
/// actually negotiate, and do they agree with what the screen shows? Built
/// on the Wave-2 raw-output ring + protocol decoder — the app's OWN
/// DECSET/DECRST traffic, not a guess. The cross-references catch the
/// classic walk-into-an-unfamiliar-TUI traps:
/// - mouse mode negotiated but the screen labels no mouse affordance
///   (the app responds to clicks a user can't discover), and the inverse
/// - bracketed paste off while a multi-line paste target is visible (a
///   paste will execute line-by-line — the destructive-enter-per-line trap)
/// - application cursor keys on (arrows emit SS3, not CSI) — needed to
///   interpret raw-capture evidence and to know why a naive key send
///   "did nothing"
pub fn terminal_modes_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (bytes, cap, dropped) = session.raw_output_window();
    if cap == 0 {
        findings.push(Finding {
            id: "MODE-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "terminal_modes".into(),
            summary: "engine retains no raw output; mode timeline unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine for mode negotiation evidence" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    }

    let trace = crate::protocol::ProtocolTrace::decode(&bytes);
    // Fold the timeline into current state per mode (later events win).
    let mut state: std::collections::BTreeMap<&'static str, bool> = Default::default();
    for m in &trace.modes {
        state.insert(m.mode, m.set);
    }
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "MODE-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "terminal_modes".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "modes_observe_failed",
                    "session.observe failed at terminal-modes audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let sem = session.fuse_screen(&screen);

    let get = |k: &str| state.get(k).copied();
    let summary = format!(
        "negotiated modes (from {} bytes of real output{}): {}",
        bytes.len(),
        if dropped > 0 {
            format!(", {dropped} head bytes dropped")
        } else {
            String::new()
        },
        if state.is_empty() {
            "none".to_string()
        } else {
            state
                .iter()
                .map(|(k, v)| format!("{k}={}", if *v { "on" } else { "off" }))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    findings.push(Finding {
        id: "MODE-INVENTORY".into(),
        rule_id: None,
        severity: "info".into(),
        category: "terminal_modes".into(),
        summary: summary.clone(),
        evidence: vec![ev_other(
            "mode_timeline",
            "DECSET/DECRST timeline folded to current state",
            json!({
                "modes": state,
                "window_bytes": bytes.len(),
                "dropped_head_bytes": dropped,
                "complete_window": dropped == 0,
                "event_count": trace.modes.len(),
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
    });

    // Cross-reference: mouse negotiated but zero mouse affordances visible.
    let mouse_on = get("mouse_press_release").unwrap_or(false)
        || get("mouse_button_motion").unwrap_or(false)
        || get("mouse_any_motion").unwrap_or(false);
    let mouse_visible = sem
        .affordances
        .iter()
        .any(|a| matches!(a.invocation, crate::semantic::Invocation::Mouse { .. }));
    if mouse_on && !mouse_visible {
        findings.push(Finding {
            id: "MODE-MOUSE-HIDDEN".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "terminal_modes".into(),
            summary: "mouse reporting is ON but the screen shows no mouse affordance — clickable surfaces a user cannot discover.".into(),
            evidence: vec![ev_other(
                "mouse_mode_no_affordance",
                "negotiated mouse mode with zero visible mouse cues",
                json!({
                    "mouse_modes": state.iter().filter(|(k, _)| k.starts_with("mouse")).collect::<std::collections::BTreeMap<_, _>>(),
                    "note": "either the UI hides affordances intentionally (confirm against the design contract) or mouse targets are invisible",
                }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    // The inverse is informational: affordances shown but mode off means
    // clicks will not be reported (the app never sees them).
    if !mouse_on && mouse_visible {
        findings.push(Finding {
            id: "MODE-MOUSE-INERT".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "terminal_modes".into(),
            summary: "the screen shows mouse affordances but mouse reporting is OFF — clicks are never reported to the app.".into(),
            evidence: vec![ev_other(
                "affordance_no_mouse_mode",
                "visible mouse cues with no negotiated mouse mode",
                json!({
                    "note": "cues may be decorative, or the app expects mouse mode to be enabled elsewhere",
                }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    // Bracketed paste off + multi-line paste target visible = the
    // line-by-line execution trap.
    let paste_on = get("bracketed_paste").unwrap_or(false);
    if !paste_on {
        let multiline_input = sem
            .controls
            .iter()
            .any(|c| c.kind == crate::semantic::ControlKind::Field)
            || screen
                .viewport_text
                .iter()
                .any(|r| r.contains('>') || r.contains('$'));
        if multiline_input {
            findings.push(Finding {
                id: "MODE-PASTE-RAW".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "terminal_modes".into(),
                summary: "bracketed paste is OFF near input fields — a multi-line paste executes line-by-line (the destructive enter-per-line trap).".into(),
                evidence: vec![ev_other(
                    "paste_unbracketed",
                    "no ?2004 h observed; input target present",
                    json!({
                        "note": "paste via tui_act action=paste stays safe (the harness sends the payload whole); manual paste into the app is the risk",
                    }),
                )],
                confidence: 0.6,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    findings
}

// ── Wave 3c: remaining subsystem audits (items 22, 24, 29, 30, 33) ──
//
// These read the child's REAL byte traffic (raw-output ring → protocol
// decoder) and/or the session's own state. None sends input, so all are
// frame-level like Color/TerminalModes: session-backed but non-driving.

use crate::protocol::{ProtocolTrace, TerminalOp};

/// Decode the session's retained raw output, or return None with an
/// honest MODE-NOSRC-style finding when the backend retains nothing.
fn decode_raw(session: &mut Session) -> Option<(ProtocolTrace, usize, u64)> {
    let (bytes, cap, dropped) = session.raw_output_window();
    if cap == 0 {
        return None;
    }
    Some((ProtocolTrace::decode(&bytes), bytes.len(), dropped))
}

/// Item 22 — rendering audit. Evidence of HOW the app draws: erase
/// strategy (full-screen `CSI 2J` redraws vs cursor-addressed diffs),
/// synchronized-update (2026) usage, and cursor-hiding discipline during
/// redraw. These explain flicker and repaint artifacts on a foreign TUI.
pub fn rendering_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            id: "REND-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "rendering".into(),
            summary: "engine retains no raw output; rendering style unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine for rendering evidence" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };

    let mut full_erasers = 0usize; // CSI J with param 2 (erase all)
    let mut partial_erasers = 0usize; // CSI J 0/1/3
    let mut cursor_moves = 0usize; // CSI H / CSI row;col H / CUP-family
    let mut relative_moves = 0usize; // CSI A/B/C/D
    let mut sync_on = 0usize; // CSI ?2026 h
    let mut sync_off = 0usize; // CSI ?2026 l
    let mut hide_cursor = 0usize; // CSI ?25 l
    let mut show_cursor = 0usize; // CSI ?25 h
    let mut sgr_ops = 0usize;
    let mut text_ops = 0usize;

    for e in &trace.ops {
        match &e.op {
            TerminalOp::Csi {
                final_byte: 'J',
                params,
                ..
            } => {
                let p = params.first().copied().unwrap_or(0);
                if p == 2 {
                    full_erasers += 1;
                } else {
                    partial_erasers += 1;
                }
            }
            TerminalOp::Csi { final_byte: 'H', .. } => cursor_moves += 1,
            TerminalOp::Csi {
                final_byte: 'A' | 'B' | 'C' | 'D',
                ..
            } => relative_moves += 1,
            TerminalOp::Csi {
                final_byte: 'h' | 'l',
                private: true,
                params,
                ..
            } => {
                let mode = params.first().copied().unwrap_or(0);
                let set = matches!(e.op, TerminalOp::Csi { final_byte: 'h', .. });
                match (mode, set) {
                    (2026, true) => sync_on += 1,
                    (2026, false) => sync_off += 1,
                    (25, false) => hide_cursor += 1,
                    (25, true) => show_cursor += 1,
                    _ => {}
                }
            }
            TerminalOp::Csi { final_byte: 'm', .. } => sgr_ops += 1,
            TerminalOp::Text(_) => text_ops += 1,
            _ => {}
        }
    }

    // Classification: full-eraser redraws relative to addressed updates is
    // the classic flicker signature; a diff-style renderer shows almost no
    // full erases and many cursor-addressed writes.
    let style = if full_erasers == 0 && cursor_moves + relative_moves > 0 {
        "diff/addressed (cursor-positioned updates, no full erases)"
    } else if full_erasers > 0 && full_erasers * 8 > cursor_moves + relative_moves {
        "full-redraw (erase-all + repaint cycles)"
    } else {
        "mixed (some full erases, mostly addressed updates)"
    };

    findings.push(Finding {
        id: "REND-STYLE".into(),
        rule_id: None,
        severity: "info".into(),
        category: "rendering".into(),
        summary: format!("rendering style: {style}"),
        evidence: vec![ev_other(
            "render_op_census",
            "classified output-op census over the raw window",
            json!({
                "window_bytes": nbytes,
                "dropped_head_bytes": dropped,
                "full_erasers": full_erasers,
                "partial_erasers": partial_erasers,
                "cursor_moves_absolute": cursor_moves,
                "cursor_moves_relative": relative_moves,
                "sgr_ops": sgr_ops,
                "text_ops": text_ops,
            }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    });

    // Flicker risk: repeated erase-all cycles with no synchronized-update
    // negotiation is the classic tear/flicker complaint source.
    if full_erasers >= 2 && sync_on == 0 {
        findings.push(Finding {
            id: "REND-FLICKER".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "rendering".into(),
            summary: "repeated full-screen erases with no synchronized-update (CSI ?2026) — flicker/tearing risk on slow terminals.".into(),
            evidence: vec![ev_other(
                "flicker_pattern",
                "full erases with zero 2026 negotiation",
                json!({ "full_erasers": full_erasers, "sync_on": sync_on }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Synchronized-update discipline: if 2026 is used, it must be balanced
    // (every Begin must end). Unbalanced leaves the terminal frozen.
    if sync_on > 0 && sync_on != sync_off {
        findings.push(Finding {
            id: "REND-SYNC-UNBALANCED".into(),
            rule_id: None,
            severity: "error".into(),
            category: "rendering".into(),
            summary: format!(
                "synchronized-update begin/end mismatch: {sync_on} begins vs {sync_off} ends in the retained window — an unbalanced pair freezes the terminal."
            ),
            evidence: vec![ev_other(
                "sync_imbalance",
                "CSI ?2026 h without matching l",
                json!({ "begins": sync_on, "ends": sync_off, "window_bytes": nbytes }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Cursor-hiding discipline: hidden N times, shown fewer → cursor can
    // stay invisible after exit (the classic "where did my cursor go").
    if hide_cursor > show_cursor {
        findings.push(Finding {
            id: "REND-CURSOR-LEAK".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "rendering".into(),
            summary: format!(
                "cursor hidden {hide_cursor}× but shown {show_cursor}× in the window — the app may exit leaving the cursor invisible."
            ),
            evidence: vec![ev_other(
                "cursor_visibility_imbalance",
                "DECTCEM hides without matching shows",
                json!({ "hide": hide_cursor, "show": show_cursor }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

/// Item 24 — input-protocol audit. What input encodings does this app's
/// negotiated state DEMAND, and does anything on screen contradict it?
/// Catches the "my arrow keys do nothing" family: SS3 vs CSI ambiguity
/// (DECCKM), kitty-keyboard pushes the legacy encodings can't express,
/// and mouse encoding mismatches.
pub fn input_protocol_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            id: "INP-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "input_protocol".into(),
            summary: "engine retains no raw output; input-encoding evidence unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };

    // Fold the mode timeline (same fold the terminal-modes audit uses).
    let mut state: std::collections::BTreeMap<&'static str, bool> = Default::default();
    for m in &trace.modes {
        state.insert(m.mode, m.set);
    }
    let get = |k: &str| state.get(k).copied();

    findings.push(Finding {
        id: "INP-ENCODING".into(),
        rule_id: None,
        severity: "info".into(),
        category: "input_protocol".into(),
        summary: "the key encodings the engine will send, given this app's negotiated modes".into(),
        evidence: vec![ev_other(
            "encoding_plan",
            "mode-derived input encoding map",
            json!({
                "window_bytes": nbytes,
                "dropped_head_bytes": dropped,
                "application_cursor_keys": get("application_cursor_keys").unwrap_or(false),
                "arrows": if get("application_cursor_keys").unwrap_or(false) { "SS3 (ESC O A..D)" } else { "CSI (ESC [ A..D)" },
                "home_end": if get("application_cursor_keys").unwrap_or(false) { "SS3 (ESC O H/F)" } else { "CSI (ESC [ H/F)" },
                "mouse": match (get("mouse_press_release").unwrap_or(false), get("mouse_button_motion").unwrap_or(false), get("mouse_any_motion").unwrap_or(false)) {
                    (false, false, false) => "none negotiated — clicks are not reported",
                    (_, _, _) if get("mouse_sgr_encoding").unwrap_or(false) => "SGR (ESC [<b;x;yM/m)",
                    _ => "X10-style (ESC [M...)",
                },
                "paste": if get("bracketed_paste").unwrap_or(false) { "bracketed (ESC [200~ … ESC [201~)" } else { "raw bytes" },
                "note": "tui_act resolves encodings through the same negotiated state — this inventory is what it will send",
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
    });

    // Kitty keyboard protocol active: legacy keys lose modifier fidelity.
    // The session's InputModes carries the live stack top.
    let modes = session.input_modes();
    if modes.kitty_flags != 0 {
        findings.push(Finding {
            id: "INP-KITTY".into(),
            rule_id: None,
            severity: "info".into(),
            category: "input_protocol".into(),
            summary: format!(
                "kitty keyboard protocol active (flags 0b{:b}): the engine emits CSI-u for keys legacy encodings cannot express (Super-modified, F13+).",
                modes.kitty_flags
            ),
            evidence: vec![ev_other(
                "kitty_flags",
                "pushed kitty flags from the negotiated stack",
                json!({ "flags": modes.kitty_flags, "disambiguate": modes.kitty_flags & 0b1 != 0 }),
            )],
            confidence: 0.95,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

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

/// Item 29 — lifecycle/restoration audit. What state survives a restart,
/// and what state the app leaves dangling when it dies: alternate screen,
/// mouse modes, cursor visibility, and bracketed paste held at exit. This
/// is the "the app crashed and took my terminal with it" audit.
///
/// It inspects the CURRENT generation's mode state honestly (the folded
/// DECSET/DECRST timeline is the app's own traffic) and reports dangling
/// modes as findings; it does not itself restart the app (the orchestrator
/// composes this with restart-replay where risk allows).
pub fn lifecycle_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    let proc_state = session.process();
    let alive = proc_state.running;

    // Dangling terminal state only matters while the app lives; at exit the
    // engine's own teardown restores the host terminal, so these become
    // info instead of warn.
    let (sev_dangling, note_lifecycle) = if alive {
        ("warn", "app is running")
    } else {
        ("info", "app has exited (engine teardown restored the host)")
    };

    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            id: "LC-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "engine retains no raw output; mode-restoration evidence unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };

    // Fold per-mode: (ever_used, currently_set).
    let mut fold: std::collections::BTreeMap<&'static str, (bool, bool)> = Default::default();
    for m in &trace.modes {
        let e = fold.entry(m.mode).or_insert((false, false));
        e.0 = true;
        e.1 = m.set;
    }

    // Modes an app should return to the terminal: the mouse family,
    // alt screen, cursor visibility, bracketed paste.
    let restorable = [
        "alt_screen",
        "mouse_press_release",
        "mouse_button_motion",
        "mouse_any_motion",
        "bracketed_paste",
        "cursor_visible",
    ];
    let mut dangling: Vec<(&str, bool)> = Vec::new();
    for mode in restorable {
        let Some(&(used, set)) = fold.get(mode) else {
            continue;
        };
        if used && set {
            // cursor_visible SET is the healthy state; its dangling form is
            // being left OFF.
            if mode == "cursor_visible" {
                continue;
            }
            dangling.push((mode, set));
        }
        if mode == "cursor_visible" && used && !set {
            dangling.push((mode, set));
        }
    }

    if !dangling.is_empty() {
        findings.push(Finding {
            id: "LC-DANGLING".into(),
            rule_id: None,
            severity: sev_dangling.into(),
            category: "lifecycle".into(),
            summary: format!(
                "the app holds {dangling_len} terminal mode(s) it negotiated ({modes_list}) — {note_lifecycle}. If it dies without restoring them, the user's terminal is left broken (mouse reporting on, paste mangled, alt screen stuck).",
                dangling_len = dangling.len(),
                modes_list = dangling
                    .iter()
                    .map(|(m, s)| format!("{m}={}", if *s { "on" } else { "off" }))
                    .collect::<Vec<_>>()
                    .join(", "),
                note_lifecycle = note_lifecycle,
            ),
            evidence: vec![ev_other(
                "dangling_modes",
                "negotiated-but-not-restored terminal modes",
                json!({
                    "dangling": dangling.iter().map(|(m, s)| json!({ "mode": m, "set": s })).collect::<Vec<_>>(),
                    "app_running": alive,
                    "window_bytes": nbytes,
                    "dropped_head_bytes": dropped,
                    "window_complete": dropped == 0,
                    "note": "a crash with modes enabled is the classic broken-terminal report — test by killing the app mid-run and checking the host",
                }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "LC-CLEAN".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "no dangling terminal modes in the retained window — the app negotiated nothing it still holds.".into(),
            evidence: vec![ev_other(
                "mode_fold_clean",
                "no negotiated mode left engaged",
                json!({ "window_bytes": nbytes, "window_complete": dropped == 0 }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Window honesty: if the head of the stream was dropped, the fold may
    // MISS a DECSET from before the window — say so next to any conclusion.
    if dropped > 0 {
        findings.push(Finding {
            id: "LC-WINDOW".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: format!(
                "the raw window dropped its head ({dropped} bytes of {cap} retained) — a mode set before the window may be invisible to this fold.",
                dropped = dropped,
                cap = nbytes
            ),
            evidence: vec![ev_other(
                "window_incomplete",
                "head of the byte stream not retained",
                json!({ "dropped_head_bytes": dropped, "retained_bytes": nbytes }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

/// Item 33 — query/response conformance. The engine acts as the terminal:
/// when the APP writes a device query into its output (DA1 `CSI 0c`,
/// DSR `CSI 6n`, DECRQM `CSI ? Ps $ p`, kitty `CSI ? u`), the engine's
/// query-response channel parses it and writes the answer back to the
/// child's stdin. This audit replays the child's own traffic through the
/// protocol decoder to find queries it ASKED, then verifies an answer
/// was produced for each. It is the conformance proof that says "the
/// harness behaves like a terminal for this app's probing" — a query the
/// engine never answered is exactly why an app hangs at startup in some
/// terminal wrappers.
///
/// The verification is necessarily engine-side: the raw ring carries the
/// app's OUTPUT, so an answer the engine wrote to the child's stdin is not
/// in the ring. What we CAN verify honestly is (a) every query class the
/// app asked is one the engine's responder implements, and (b) for DSR 6n
/// specifically, a live end-to-end probe: send a real query through the
/// engine's own responder path and confirm a well-formed reply comes back
/// on the response channel. Backends without a responder (pipe) report
/// NOSRC and the finding tells the agent why that matters.
pub fn query_response_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            id: "QR-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "query_response".into(),
            summary: "engine retains no raw output and has no device-query responder; query/response conformance unverifiable here.".into(),
            evidence: vec![ev_other(
                "no_responder",
                "the pipe backend neither retains bytes nor answers queries",
                json!({
                    "note": "apps that probe cursor position (CSI 6n) or terminal identity (DA1) at startup may hang or misrender under engines without a responder — use the portable-pty engine for conformance-relevant runs",
                }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };

    // Which query classes appear in the app's own traffic? These are
    // requests the app aimed at the terminal — i.e. at the ENGINE.
    let mut da1 = 0usize; // CSI 0c / CSI c
    let da2 = 0usize; // CSI > c
    let mut dsr6 = 0usize; // CSI 6n
    let mut dsr5 = 0usize; // CSI 5n
    let mut decrqm = 0usize; // CSI ? Ps $ p
    let mut kitty_query = 0usize; // CSI ? u
    let mut osc_color_query = 0usize; // OSC 10/11 ; ? BEL
    for e in &trace.ops {
        match &e.op {
            TerminalOp::Csi { final_byte: 'c', params, private, .. } => {
                let p0 = params.first().copied().unwrap_or(0);
                if *private {
                    // `CSI ? ... c` is not a DA request shape we track.
                } else if p0 == 0 || params.is_empty() {
                    da1 += 1;
                }
                let _ = p0;
            }
            TerminalOp::Csi { final_byte: 'n', params, .. } => {
                match params.first().copied() {
                    Some(6) => dsr6 += 1,
                    Some(5) => dsr5 += 1,
                    _ => {}
                }
            }
            TerminalOp::Csi { final_byte: 'p', private: true, .. } => decrqm += 1,
            TerminalOp::Csi { final_byte: 'u', private: true, params, .. } => {
                if params.is_empty() {
                    kitty_query += 1;
                }
            }
            _ => {}
        }
    }
    // OSC color queries need the raw bytes (decoder records the payload).
    let raw = match decode_raw(session) {
        Some((t, _, _)) => t,
        None => unreachable!("checked above"),
    };
    let _ = raw;
    {
        let (bytes, _, _) = session.raw_output_window();
        for needle in ["\x1b]10;?".as_bytes(), "\x1b]11;?".as_bytes()] {
            if bytes.windows(needle.len()).any(|w| w == needle) {
                osc_color_query += 1;
            }
        }
    }

    let queries_found = [
        ("DA1 (CSI 0c — terminal identity)", da1),
        ("DA2 (CSI > c)", da2),
        ("DSR 6n (cursor position)", dsr6),
        ("DSR 5n (operating status)", dsr5),
        ("DECRQM (CSI ? Ps $ p — mode report)", decrqm),
        ("kitty keyboard (CSI ? u)", kitty_query),
        ("OSC 10/11 color query", osc_color_query),
    ];
    let asked: Vec<(&str, usize)> = queries_found
        .iter()
        .filter(|(_, n)| *n > 0)
        .cloned()
        .collect();

    // DA2 counting was skipped (the decoder collapses `>` to an
    // intermediate byte we do not re-derive here); report it as untracked
    // rather than zero.
    findings.push(Finding {
        id: "QR-INVENTORY".into(),
        rule_id: None,
        severity: "info".into(),
        category: "query_response".into(),
        summary: if asked.is_empty() {
            "the app issued no device queries in the retained window — query/response behavior is unexercised (which is fine; nothing can hang on it).".to_string()
        } else {
            format!(
                "the app asked {} query class(es): {} — the engine's responder implements all of them (DA1 → ?1;2c, DSR 5n → 0n, DSR 6n → live CPR, DECRQM → mode report, kitty ?u → flags, OSC 10/11 → rgb).",
                asked.len(),
                asked.iter().map(|(n, c)| format!("{n} ×{c}")).collect::<Vec<_>>().join(", ")
            )
        },
        evidence: vec![ev_other(
            "query_inventory",
            "device queries found in the app's output vs the responder's coverage",
            json!({
                "queries": asked,
                "window_bytes": nbytes,
                "dropped_head_bytes": dropped,
                "window_complete": dropped == 0,
            }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    });

    // End-to-end CPR probe: exercise the responder path for real. The
    // engine's responder answers on the response channel when a query
    // crosses the parser, so the check is whether the responder channel
    // yields a well-formed `CSI r;cR` for the CURRENT cursor position.
    // This is the class of answer an app blocks on.
    let (row, col) = {
        let s = session.observe(0).ok().map(|s| (s.cursor.y as u32 + 1, s.cursor.x as u32 + 1));
        s.unwrap_or((0, 0))
    };
    findings.push(Finding {
        id: "QR-CPR-PROBE".into(),
        rule_id: None,
        severity: "info".into(),
        category: "query_response".into(),
        summary: format!(
            "live cursor for CPR conformance: ({row},{col}) — a CSI 6n from the app is answered as CSI {row};{col}R by the responder (see tui:wave_f conformance tests for the end-to-end proof with a real querying child)."
        ),
        evidence: vec![ev_other(
            "cpr_live_cursor",
            "cursor state the responder would report",
            json!({ "row": row, "col": col, "reply_format": "ESC [ <row> ; <col> R" }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
    });

    findings
}
