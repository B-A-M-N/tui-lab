//! Visual family: color-scheme handling (item 24) and rendering
//! strategy evidence from real byte traffic (item 22).
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::session::state::Session;
use serde_json::json;

use crate::audit::{Category, Finding, Severity};
use crate::protocol::TerminalOp;

use super::shared::{decode_raw, ev_other, ev_other_empty};

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
                severity: Severity::Error,
                category: Category::Color,
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "color_observe_failed",
                    "session.observe failed at color audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
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
        severity: Severity::Info,
        category: Category::Color,
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
        occurrence_id: None,
    });
    findings
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
            severity: Severity::Info,
            category: Category::Rendering,
            summary: "engine retains no raw output; rendering style unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine for rendering evidence" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
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
            TerminalOp::Csi {
                final_byte: 'H', ..
            } => cursor_moves += 1,
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
                let set = matches!(
                    e.op,
                    TerminalOp::Csi {
                        final_byte: 'h',
                        ..
                    }
                );
                match (mode, set) {
                    (2026, true) => sync_on += 1,
                    (2026, false) => sync_off += 1,
                    (25, false) => hide_cursor += 1,
                    (25, true) => show_cursor += 1,
                    _ => {}
                }
            }
            TerminalOp::Csi {
                final_byte: 'm', ..
            } => sgr_ops += 1,
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
        severity: Severity::Info,
        category: Category::Rendering,
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
        occurrence_id: None,
    });

    // Flicker risk: repeated erase-all cycles with no synchronized-update
    // negotiation is the classic tear/flicker complaint source.
    if full_erasers >= 2 && sync_on == 0 {
        findings.push(Finding {
            id: "REND-FLICKER".into(),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::Rendering,
            summary: "repeated full-screen erases with no synchronized-update (CSI ?2026) — flicker/tearing risk on slow terminals.".into(),
            evidence: vec![ev_other(
                "flicker_pattern",
                "full erases with zero 2026 negotiation",
                json!({ "full_erasers": full_erasers, "sync_on": sync_on }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    // Synchronized-update discipline: if 2026 is used, it must be balanced
    // (every Begin must end). Unbalanced leaves the terminal frozen.
    // Incomplete-window rule (re-review item 20): when the raw ring dropped
    // its head, an imbalance inside this window is NOT proof — the matching
    // end may have scrolled out. Such a verdict becomes `unverified`.
    if sync_on > 0 && sync_on != sync_off {
        let (severity, summary, confidence) = if dropped > 0 {
            (
                Severity::Info,
                format!(
                    "synchronized-update begin/end mismatch in the retained window ({sync_on} begins vs {sync_off} ends) — window is incomplete, so this is UNVERIFIED, not proof of an unbalanced pair."
                ),
                0.4,
            )
        } else {
            (
                Severity::Error,
                format!(
                    "synchronized-update begin/end mismatch: {sync_on} begins vs {sync_off} ends in the retained window — an unbalanced pair freezes the terminal."
                ),
                0.85,
            )
        };
        findings.push(Finding {
            id: "REND-SYNC-UNBALANCED".into(),
            rule_id: None,
            severity,
            category: Category::Rendering,
            summary,
            evidence: vec![ev_other(
                "sync_imbalance",
                "CSI ?2026 h without matching l",
                json!({
                    "begins": sync_on, "ends": sync_off, "window_bytes": nbytes,
                    "dropped_head_bytes": dropped,
                    "verdict": if dropped > 0 { "unverified" } else { "error" },
                }),
            )],
            confidence,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    // Cursor-hiding discipline: hidden N times, shown fewer → cursor can
    // stay invisible after exit (the classic "where did my cursor go").
    // Incomplete-window rule (re-review item 20): a dropped head makes this
    // unverified — the matching show may predate the window.
    if hide_cursor > show_cursor {
        let (severity, summary, confidence) = if dropped > 0 {
            (
                Severity::Info,
                format!(
                    "cursor hidden {hide_cursor}× but shown {show_cursor}× in the window — window is incomplete, so this is UNVERIFIED."
                ),
                0.4,
            )
        } else {
            (
                Severity::Warn,
                format!(
                    "cursor hidden {hide_cursor}× but shown {show_cursor}× in the window — the app may exit leaving the cursor invisible."
                ),
                0.7,
            )
        };
        findings.push(Finding {
            id: "REND-CURSOR-LEAK".into(),
            rule_id: None,
            severity,
            category: Category::Rendering,
            summary,
            evidence: vec![ev_other(
                "cursor_visibility_imbalance",
                "DECTCEM hides without matching shows",
                json!({
                    "hide": hide_cursor, "show": show_cursor,
                    "dropped_head_bytes": dropped,
                    "verdict": if dropped > 0 { "unverified" } else { "warn" },
                }),
            )],
            confidence,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    findings
}
