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
    let baseline_sem = match session.observe(50) {
        Ok(s) => semantic::analyze(&s),
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
                reproduction: None,
            });
            return findings;
        }
    };
    let mut last_id = baseline_sem.focus.control_id.clone();

    let mut visited_states: Vec<String> = Vec::new();
    let mut focus_order: Vec<Option<String>> = Vec::new();
    let mut successful_tabs = 0u32;

    for i in 0..max_tabs {
        let before = match session.observe(30) {
            Ok(s) => s,
            Err(_) => break,
        };
        let sem_before = semantic::analyze(&before);
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
                reproduction: None,
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
                reproduction: None,
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
            });
        }
        if successful_tabs > 0 && reverse_ok {
            findings.push(Finding {
                id: "KB-OK".into(),
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
                reproduction: None,
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
            reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                    reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
                reproduction: None,
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
            reproduction: None,
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
    });

    if !gaps.is_empty() {
        findings.push(Finding {
            id: "NAV-REVERSE-GAP".into(),
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
        });
    } else {
        findings.push(Finding {
            id: "NAV-REVERSE-OK".into(),
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
                severity: "error".into(),
                category: "mouse".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "mouse_observe_failed",
                    "session.observe failed at mouse audit start",
                )],
                confidence: 1.0,
                reproduction: None,
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    if !caps.mouse {
        findings.push(Finding {
            id: "MOUSE-NO-CAPS".into(),
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
        });
        // Continue anyway: an app with mouse support behind a
        // capability-blind backend is still worth probing honestly.
    }

    let sem = semantic::analyze(&screen);
    // Risk filter (Wave D risk classes): only click SAFE-looking targets —
    // buttons and links whose click might mutate or destroy state are
    // listed, not clicked, unless the screen marks them unambiguous. Here
    // we click only controls whose label does not match destructive verbs.
    let destructive = ["delete", "remove", "quit", "kill", "reset", "format", "erase"];
    let clickable: Vec<&semantic::Control> = sem
        .controls
        .iter()
        .filter(|c| {
            if !c.enabled || c.bounds.width == 0 || c.bounds.height == 0 {
                return false;
            }
            matches!(c.kind, semantic::ControlKind::Button | semantic::ControlKind::Tab | semantic::ControlKind::MenuItem)
                || c.focusable
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
        });
        return findings;
    }

    let mut clicked = 0u32;
    let mut unresponsive: Vec<String> = Vec::new();
    let mut responded: Vec<String> = Vec::new();

    for ctrl in &clickable {
        let cx = ctrl.bounds.x.saturating_add(ctrl.bounds.width / 2);
        let cy = ctrl.bounds.y.saturating_add(ctrl.bounds.height / 2);
        let before = match session.observe(30) {
            Ok(s) => s,
            Err(_) => break,
        };
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
                });
                break;
            }
        };
        clicked += 1;
        let before_sem = semantic::analyze(&before);
        let after_sem = semantic::analyze(tx.after());
        let focus_moved = before_sem.focus.control != after_sem.focus.control;
        let structure_changed = before.structure_hash != tx.after().structure_hash;
        if focus_moved || structure_changed {
            responded.push(ctrl.label.clone());
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
                    "note": "unresponsive = neither focus nor structure hash changed within the settle budget; the control may still act non-visibly",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
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
                    severity: "error".into(),
                    category: "performance".into(),
                    summary: format!("Observe failed during sampling: {}", e),
                    evidence: vec![ev_other_empty(
                        "perf_observe_failed",
                        "session.observe failed during performance sampling",
                    )],
                    confidence: 1.0,
                    reproduction: None,
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
    let settle_saturated = settle_ms.iter().filter(|&&m| m >= SETTLE_CEILING).count() * 2 >= settle_ms.len();
    findings.push(Finding {
        id: if slow_observe { "PERF-OBSERVE-SLOW" } else { "PERF-OK" }.into(),
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
    });
    if settle_saturated {
        findings.push(Finding {
            id: "PERF-NEVER-QUIET".into(),
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
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                id: "STATES-ERR".into(),
                severity: "error".into(),
                category: "states".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "states_observe_failed",
                    "session.observe failed at states audit start",
                )],
                confidence: 1.0,
                reproduction: None,
            });
            return findings;
        }
    };
    let sem = semantic::analyze(&screen);
    let disabled: Vec<&semantic::Control> =
        sem.controls.iter().filter(|c| !c.enabled).collect();
    let empty_like: Vec<&semantic::Control> = sem
        .controls
        .iter()
        .filter(|c| c.enabled && c.label.trim().is_empty())
        .collect();

    if !disabled.is_empty() {
        findings.push(Finding {
            id: "STATES-DISABLED".into(),
            severity: "info".into(),
            category: "states".into(),
            summary: format!(
                "{} disabled control(s) on this screen: {}",
                disabled.len(),
                disabled.iter().map(|c| c.label.clone()).collect::<Vec<_>>().join(", ")
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
        });
    }
    if !empty_like.is_empty() {
        findings.push(Finding {
            id: "STATES-EMPTY-CONTROLS".into(),
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
            });
        }
    }

    if findings.is_empty() {
        findings.push(Finding {
            id: "STATES-OK".into(),
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
                severity: "error".into(),
                category: "errors".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "errors_observe_failed",
                    "session.observe failed at errors audit start",
                )],
                confidence: 1.0,
                reproduction: None,
            });
            return findings;
        }
    };

    // (1) Crash resistance: a bounded burst of safe keys.
    let safe_keys = [
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Escape,
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
                    "key_classes": "tab/shift+tab/arrows/escape",
                    "exit_state": after.as_ref().ok().map(|s| &s.process),
                }),
            )],
            confidence: 0.9,
            reproduction: None,
        });
        // Skip the error scan on a dead screen; the crash IS the finding.
        return findings;
    }
    let _ = exited_clean;

    // (2) On-screen error scan of the final frame.
    if let Ok(s) = after {
        let joined = s.viewport_text.join("\n");
        let lowered = joined.to_lowercase();
        let mut markers: Vec<&str> = Vec::new();
        for m in [
            "panicked at",
            "traceback (most recent call last)",
            "unhandled exception",
            "segmentation fault",
            "error:",
            "fatal:",
            "exception:",
        ] {
            if lowered.contains(m) {
                markers.push(m);
            }
        }
        if !markers.is_empty() {
            // Pull the offending lines as evidence (bounded).
            let lines: Vec<String> = s
                .viewport_text
                .iter()
                .filter(|l| {
                    let ll = l.to_lowercase();
                    markers.iter().any(|m| ll.contains(m))
                })
                .take(5)
                .cloned()
                .collect();
            findings.push(Finding {
                id: "ERR-ON-SCREEN".into(),
                severity: "error".into(),
                category: "errors".into(),
                summary: format!(
                    "Error text visible on screen: {} marker(s) found ({})",
                    markers.len(),
                    markers.join(", ")
                ),
                evidence: vec![ev_other(
                    "error_text_on_screen",
                    "viewport lines matching error-shaped markers",
                    json!({
                        "markers": markers,
                        "lines": lines,
                        "structure_hash": s.structure_hash,
                    }),
                )],
                confidence: 0.85,
                reproduction: None,
            });
        } else {
            findings.push(Finding {
                id: "ERR-OK".into(),
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
                severity: "error".into(),
                category: "color".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "color_observe_failed",
                    "session.observe failed at color audit start",
                )],
                confidence: 1.0,
                reproduction: None,
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    let styled_cells = screen.cells.iter().filter(|c| c.fg.rgb.is_some() || c.fg.palette.is_some() || c.bg.rgb.is_some() || c.bg.palette.is_some()).count();
    findings.push(Finding {
        id: "COLOR-INVENTORY".into(),
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
    });
    findings
}
