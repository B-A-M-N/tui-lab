//! Layout & navigation family: clipping (spec item 55), focus-navigation
//! graph walks, and navigation-key handling.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::KeyEvent;
use crate::execution::{execute_act_as, CanonicalAction};
use crate::semantic;
use crate::session::state::Session;
use serde_json::json;

use super::keyboard::keyboard_audit;
use super::shared::{ev_other, ev_other_empty, has_incomplete_border};
use crate::audit::Finding;

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

/// Item 24 — navigation BEYOND Tab. The Tab audit proves the Tab cycle and
/// its Shift+Tab inverse; real TUIs navigate with arrows, Home/End and
/// PageUp/PageDown too, and each of those has its own reverse-consistency
/// contract (Left↔Right, Up↔Down, Home↔End, PageUp↔PageDown). This driver
/// walks each key class through the canonical executor, records its edges
/// in the shared focus graph, and reports per class:
///
/// - coverage (how many transitions the class produced);
/// - trap: the key never moved focus although several focusable controls
///   were visible;
/// - reverse consistency: the class's inverse key must retrace the
///   forward walk (proved edge-for-edge on stable IDs, like Shift+Tab).
///
/// Key classes and their inverses:
///   left ↔ right, up ↔ down, home ↔ end, pageup ↔ pagedown.
pub fn navigation_keys_audit(session: &mut Session, steps_per_class: u32) -> Vec<Finding> {
    use crate::backend::KeyCode;

    /// One navigation class: forward key, inverse key, graph `via` names.
    struct KeyClass {
        name: &'static str,
        forward: KeyEvent,
        inverse: KeyEvent,
        forward_via: &'static str,
        inverse_via: &'static str,
    }

    let classes = [
        KeyClass {
            name: "left/right",
            forward: KeyEvent::new(KeyCode::Right),
            inverse: KeyEvent::new(KeyCode::Left),
            forward_via: "right",
            inverse_via: "left",
        },
        KeyClass {
            name: "up/down",
            forward: KeyEvent::new(KeyCode::Down),
            inverse: KeyEvent::new(KeyCode::Up),
            forward_via: "down",
            inverse_via: "up",
        },
        KeyClass {
            name: "home/end",
            forward: KeyEvent::new(KeyCode::End),
            inverse: KeyEvent::new(KeyCode::Home),
            forward_via: "end",
            inverse_via: "home",
        },
        KeyClass {
            name: "pageup/pagedown",
            forward: KeyEvent::new(KeyCode::PageDown),
            inverse: KeyEvent::new(KeyCode::PageUp),
            forward_via: "pagedown",
            inverse_via: "pageup",
        },
    ];

    let mut findings = Vec::new();
    let mut graph = crate::semantic::focus_graph::FocusGraph::new();

    for class in &classes {
        let mut moved = 0u32;
        let mut focus_walk: Vec<Option<String>> = Vec::new();
        let mut trapped_at: Option<u32> = None;
        for i in 0..steps_per_class {
            let (_, sem_before, _, _) = match session.observe_fused(30) {
                Ok(t) => t,
                Err(_) => break,
            };
            let focusable = sem_before.controls.iter().filter(|c| c.focusable).count();
            let before_id = sem_before.focus.control_id.clone();
            let tx = match execute_act_as(
                session,
                crate::execution::DriveOrigin::Audit,
                &CanonicalAction::Key { key: class.forward },
                60,
                400,
                false,
            ) {
                Ok(t) => t,
                Err(_) => break,
            };
            let sem_after = session.fuse_screen(tx.after());
            let after_id = sem_after.focus.control_id.clone();
            focus_walk.push(after_id.clone());
            if let (Some(f), Some(t)) = (&before_id, &after_id) {
                graph.record_edge(f, t, class.forward_via, sem_after.focus.control.as_deref());
            }
            if before_id != after_id && before_id.is_some() {
                moved += 1;
            } else if focusable > 1 && trapped_at.is_none() {
                trapped_at = Some(i);
            }
        }

        // Reverse walk: the inverse key should retrace the forward edges.
        let mut retraced = 0u32;
        for _ in 0..moved.max(1) {
            let (_, sem_before, _, _) = match session.observe_fused(30) {
                Ok(t) => t,
                Err(_) => break,
            };
            let before_id = sem_before.focus.control_id.clone();
            let Ok(tx) = execute_act_as(
                session,
                crate::execution::DriveOrigin::Audit,
                &CanonicalAction::Key { key: class.inverse },
                60,
                400,
                false,
            ) else {
                break;
            };
            let sem_after = session.fuse_screen(tx.after());
            let after_id = sem_after.focus.control_id.clone();
            if let (Some(f), Some(t)) = (&before_id, &after_id) {
                graph.record_edge(f, t, class.inverse_via, sem_after.focus.control.as_deref());
            }
            if before_id != after_id {
                retraced += 1;
            }
        }

        // Per-class findings.
        if moved == 0 {
            findings.push(Finding {
                id: format!("NAV-{}-UNUSED", class.name.to_uppercase().replace('/', "-")),
                rule_id: None,
                severity: "info".into(),
                category: "navigation".into(),
                summary: format!(
                    "{} navigation produced no focus transitions in {} steps — either the app does not use these keys or focus does not visibly track them.",
                    class.name, steps_per_class
                ),
                evidence: vec![ev_other(
                    "nav_class_unused",
                    "no focus change from this key class",
                    json!({ "class": class.name, "steps": steps_per_class }),
                )],
                confidence: 0.7,
                reproduction: None,
                source_refs: Vec::new(),
            });
            continue;
        }

        if let Some(step) = trapped_at {
            findings.push(Finding {
                id: format!("NAV-{}-TRAP", class.name.to_uppercase().replace('/', "-")),
                rule_id: None,
                severity: "warn".into(),
                category: "navigation".into(),
                summary: format!(
                    "{} stopped changing focus at step {} although multiple focusable controls are visible — a navigation trap for this key class.",
                    class.name, step
                ),
                evidence: vec![ev_other(
                    "nav_class_trap",
                    "focus stopped moving under a non-degenerate screen",
                    json!({ "class": class.name, "step": step, "walk": focus_walk }),
                )],
                confidence: 0.8,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }

        // Reverse consistency edge-for-edge: every forward edge needs its
        // inverse counterpart (same pairing rule as Shift+Tab vs Tab).
        let gaps: Vec<(String, String)> = graph
            .edges
            .iter()
            .filter(|e| e.via == class.forward_via)
            .filter(|e| {
                !graph
                    .edges
                    .iter()
                    .any(|r| r.from == e.to && r.to == e.from && r.via == class.inverse_via)
            })
            .map(|e| (e.to.clone(), e.from.clone()))
            .collect();
        if gaps.is_empty() {
            findings.push(Finding {
                id: format!("NAV-{}-REVERSE-OK", class.name.to_uppercase().replace('/', "-")),
                rule_id: None,
                severity: "info".into(),
                category: "navigation".into(),
                summary: format!(
                    "{} navigation is reversible: {} forward transition(s) and each has its {} inverse.",
                    class.name, moved, class.inverse_via
                ),
                evidence: vec![ev_other(
                    "nav_class_reverse_ok",
                    "edge-for-edge inverse holds",
                    json!({ "class": class.name, "forward_moves": moved, "inverse_moves": retraced }),
                )],
                confidence: 0.85,
                reproduction: None,
                source_refs: Vec::new(),
            });
        } else {
            findings.push(Finding {
                id: format!("NAV-{}-REVERSE-GAP", class.name.to_uppercase().replace('/', "-")),
                rule_id: None,
                severity: "warn".into(),
                category: "navigation".into(),
                summary: format!(
                    "{} navigation is not reversible: {} forward edge(s) lack a {} inverse — keyboard users navigating back land elsewhere.",
                    class.name, gaps.len(), class.inverse_via
                ),
                evidence: vec![ev_other(
                    "nav_class_reverse_gaps",
                    "forward edges without their inverse counterpart (stable IDs)",
                    json!({ "class": class.name, "gaps": gaps }),
                )],
                confidence: 0.85,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }

        // Unreachable controls: focusable controls the class never reached.
        let (_, sem_now, _, _) = match session.observe_fused(30) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let reached: std::collections::HashSet<&str> = graph
            .edges
            .iter()
            .filter(|e| e.via == class.forward_via || e.via == class.inverse_via)
            .flat_map(|e| [e.from.as_str(), e.to.as_str()])
            .collect();
        let unreachable: Vec<String> = sem_now
            .controls
            .iter()
            .filter(|c| c.focusable)
            .filter(|c| !reached.contains(c.id.as_str()))
            .map(|c| c.id.clone())
            .collect();
        if !unreachable.is_empty() && moved > 0 {
            findings.push(Finding {
                id: format!("NAV-{}-UNREACHABLE", class.name.to_uppercase().replace('/', "-")),
                rule_id: None,
                severity: "info".into(),
                category: "navigation".into(),
                summary: format!(
                    "{} focusable control(s) were never reached by {} navigation: {} — reachable by other means (Tab/mouse) or genuinely stranded.",
                    unreachable.len(), class.name, unreachable.join(", ")
                ),
                evidence: vec![ev_other(
                    "nav_class_unreachable",
                    "focusable controls outside this class's visited set",
                    json!({ "class": class.name, "unreachable": unreachable }),
                )],
                confidence: 0.7,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }

    // Merge the class graph into the session-scoped one the caller owns?
    // This driver owns a private graph because its via-names are class-
    // specific; return the merged view in evidence for cross-run queries.
    findings.push(Finding {
        id: "NAV-KEYS-SUMMARY".into(),
        rule_id: None,
        severity: "info".into(),
        category: "navigation".into(),
        summary: format!(
            "extended navigation coverage: {} edge(s) recorded across arrows/home-end/pageup-pagedown classes.",
            graph.edges.len()
        ),
        evidence: vec![ev_other(
            "nav_keys_graph",
            "per-class focus edges (stable IDs)",
            json!({ "graph": graph.summary() }),
        )],
        confidence: 1.0,
        reproduction: None,
        source_refs: Vec::new(),
    });

    findings
}
