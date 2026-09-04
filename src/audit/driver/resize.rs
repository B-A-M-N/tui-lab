//! Resize family (spec items 53, 54-adjacent): viewport-resize matrix and
//! reflow-after-resize behavior.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::WaitCond;
use crate::session::state::Session;
use serde_json::json;

use super::shared::{ev_other, ev_other_empty};
use crate::audit::Finding;
const RESIZE_MATRIX: &[(u16, u16)] = &[(60, 20), (80, 24), (100, 30), (120, 40), (160, 50)];

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

/// Item 25 — resize SHRINK→GROW reflow invariants. The matrix audit checks
/// each size independently; what it cannot see is the ROUND TRIP: content
/// visible at the original size that vanishes on shrink and never comes
/// back on grow (lost rows), focus that the app fails to restore, and
/// ghost cells left behind in the grown-back area. Sequence:
///
///   baseline @ WxH  →  shrink to W/2xH/2  →  grow back to WxH
///
/// and each invariant is reported against measured before/after frames.
pub fn resize_reflow_audit(session: &mut Session) -> Vec<Finding> {
    use std::time::Duration;

    let mut findings = Vec::new();
    let (w, h) = (session.cols(), session.rows());
    // Skip degenerate originals: halving 40x12 would give 20x6 — legal, but
    // below ~10 columns semantic analysis is noise. Report honestly.
    if w < 20 || h < 8 {
        findings.push(Finding {
            id: "RFLW-TOO-SMALL".into(),
            rule_id: None,
            severity: "info".into(),
            category: "resize".into(),
            summary: format!(
                "session is {w}x{h}; the shrink→grow probe needs ≥20x8 to halve meaningfully — skipped."
            ),
            evidence: vec![ev_other(
                "reflow_skipped",
                "original size too small to halve",
                json!({ "cols": w, "rows": h }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    }

    let settle = |s: &mut Session, quiet: u64| {
        let _ = s.wait(
            WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(quiet),
                after_screen_seq: None,
            },
            2000,
        );
    };

    let base = match session.observe_fused(120) {
        Ok((s, sem, _, _)) => (s, sem),
        Err(e) => {
            findings.push(Finding {
                id: "RFLW-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "resize".into(),
                summary: format!("baseline observe failed: {e}"),
                evidence: vec![ev_other_empty("reflow_baseline_failed", "observe failed")],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let base_focus = base.1.focus.control_id.clone();
    let base_nonblank_rows: Vec<usize> = base
        .0
        .viewport_text
        .iter()
        .enumerate()
        .filter(|(_, r)| r.trim().len() >= 4)
        .map(|(i, _)| i)
        .collect();

    // ── Shrink ────────────────────────────────────────────────────────────
    let (sw, sh) = (w / 2, h / 2);
    if let Err(e) = session.resize(sw, sh) {
        findings.push(Finding {
            id: "RFLW-ERR".into(),
            rule_id: None,
            severity: "error".into(),
            category: "resize".into(),
            summary: format!("shrink to {sw}x{sh} failed: {e}"),
            evidence: vec![ev_other_empty(
                "reflow_shrink_failed",
                "resize returned Err",
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        let _ = session.resize(w, h);
        return findings;
    }
    settle(session, 150);
    let shrunken = session.observe_fused(120).ok();
    let shrink_focus = shrunken.as_ref().and_then(|t| t.1.focus.control_id.clone());
    let (_shru_screen, shru_sem) = match shrunken {
        Some(t) => (t.0, t.1),
        None => {
            findings.push(Finding {
                id: "RFLW-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "resize".into(),
                summary: "observe after shrink failed".into(),
                evidence: vec![ev_other_empty(
                    "reflow_shrink_observe_failed",
                    "observe failed",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            let _ = session.resize(w, h);
            return findings;
        }
    };
    let clipped_at_shrink: Vec<String> = shru_sem
        .regions
        .iter()
        .filter(|rg| !matches!(rg.clipping_state, crate::semantic::ClippingState::None))
        .map(|rg| rg.id.clone())
        .collect();
    if !clipped_at_shrink.is_empty() {
        findings.push(Finding {
            id: "RFLW-SHRINK-CLIP".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "resize".into(),
            summary: format!(
                "at {sw}x{sh} (half the original {w}x{h}), {} region(s) clip: {} — a small-window user loses content.",
                clipped_at_shrink.len(),
                clipped_at_shrink.join(", ")
            ),
            evidence: vec![ev_other(
                "reflow_shrink_clipping",
                "clipped regions at the halved size",
                json!({ "cols": sw, "rows": sh, "clipped": clipped_at_shrink }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // ── Grow back ─────────────────────────────────────────────────────────
    let _ = session.resize(w, h);
    settle(session, 150);
    let grown = session.observe_fused(120);
    let (grw_screen, grw_sem) = match grown {
        Ok(t) => (t.0, t.1),
        Err(e) => {
            findings.push(Finding {
                id: "RFLW-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "resize".into(),
                summary: format!("observe after grow-back failed: {e}"),
                evidence: vec![ev_other_empty(
                    "reflow_grow_observe_failed",
                    "observe failed",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };

    // Invariant 1: content survives the round trip. Every baseline row
    // with ≥4 non-blank chars must have its text present somewhere in the
    // grown-back screen (app may rewrap, so compare text membership per
    // trimmed row, not row index).
    let grw_joined: String = grw_screen
        .viewport_text
        .iter()
        .map(|r: &String| r.trim().to_string())
        .filter(|r: &String| !r.is_empty())
        .collect::<Vec<String>>()
        .join("\n");
    let mut lost_rows: Vec<String> = Vec::new();
    for i in &base_nonblank_rows {
        let text = base.0.viewport_text[*i].trim().to_string();
        if text.is_empty() || !grw_joined.contains(&text) {
            lost_rows.push(format!("row {i}: {text:?}"));
        }
    }
    if !lost_rows.is_empty() {
        findings.push(Finding {
            id: "RFLW-CONTENT-LOST".into(),
            rule_id: None,
            severity: "error".into(),
            category: "resize".into(),
            summary: format!(
                "{} baseline row(s) never reappeared after shrink→grow ({}x{} → {}x{} → {}x{}): {} — the app's reflow loses content permanently.",
                lost_rows.len(), w, h, sw, sh, w, h,
                lost_rows.join("; ")
            ),
            evidence: vec![ev_other(
                "reflow_content_lost",
                "baseline rows absent from the grown-back frame",
                json!({
                    "original": { "cols": w, "rows": h },
                    "shrunken": { "cols": sw, "rows": sh },
                    "lost_rows": lost_rows,
                }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "RFLW-CONTENT-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "resize".into(),
            summary: format!(
                "content survives the round trip: all {} content row(s) present after {w}x{h} → {sw}x{sh} → {w}x{h}.",
                base_nonblank_rows.len()
            ),
            evidence: vec![ev_other(
                "reflow_content_ok",
                "every content row reappears after grow-back",
                json!({ "rows_checked": base_nonblank_rows.len() }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Invariant 2: focus survives (or is honestly re-homed). Compare IDs
    // when both sides name one; a missing focus at the end is a defect
    // only if the baseline HAD one.
    if let (Some(before), Some(after)) = (&base_focus, &grw_sem.focus.control_id) {
        if before != after {
            findings.push(Finding {
                id: "RFLW-FOCUS-MOVED".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "resize".into(),
                summary: format!(
                    "focus changed across the round trip: {before:?} → {after:?} (the app did not restore the focused control)."
                ),
                evidence: vec![ev_other(
                    "reflow_focus_moved",
                    "focus id at baseline vs after grow-back (stable IDs)",
                    json!({ "before": before, "after": after, "at_shrink": shrink_focus }),
                )],
                confidence: 0.8,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    } else if base_focus.is_some() && grw_sem.focus.control_id.is_none() {
        findings.push(Finding {
            id: "RFLW-FOCUS-LOST".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "resize".into(),
            summary: "the app had a focused control at baseline and none after shrink→grow — keyboard users start from nothing.".into(),
            evidence: vec![ev_other(
                "reflow_focus_lost",
                "focus present before, absent after",
                json!({ "before": base_focus, "at_shrink": shrink_focus }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "RFLW-FOCUS-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "resize".into(),
            summary: "focus state survives the round trip (both ends agree, or neither had a focus).".into(),
            evidence: vec![ev_other(
                "reflow_focus_ok",
                "focus before/at-shrink/after",
                json!({ "before": base_focus, "at_shrink": shrink_focus, "after": grw_sem.focus.control_id }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Invariant 3: no ghost cells — after grow-back, rows the app owns
    // (content rows at baseline) must not carry trailing garbage beyond
    // what the baseline had. Compare the max non-space column per content
    // region: a grown-back row substantially wider than its baseline
    // counterpart means leftover pixels from the shrunken layout.
    let ghost_rows: Vec<usize> = grw_screen
        .viewport_text
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            let t = r.trim_end();
            // "Ghost" heuristic: trailing fragments after wide blank runs
            // mid-row (content — 8+ spaces — non-space — end).
            if let Some(pos) = t.rfind("        ") {
                let tail = &t[pos + 8..];
                !tail.trim().is_empty() && tail.trim().chars().all(|c| c.is_ascii_graphic())
            } else {
                false
            }
        })
        .map(|(i, _)| i)
        .collect();
    let base_ghostish = base
        .0
        .viewport_text
        .iter()
        .filter(|r| {
            let t = r.trim_end();
            matches!(t.rfind("        "), Some(pos) if !t[pos + 8..].trim().is_empty())
        })
        .count();
    if ghost_rows.len() > base_ghostish {
        findings.push(Finding {
            id: "RFLW-GHOST-CELLS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "resize".into(),
            summary: format!(
                "{} row(s) show fragmented remnants after grow-back (baseline had {base_ghostish}) — possible ghost cells from an incomplete repaint.",
                ghost_rows.len()
            ),
            evidence: vec![ev_other(
                "reflow_ghost_candidates",
                "rows with wide-gap + trailing fragment shape after grow-back",
                json!({
                    "rows": ghost_rows,
                    "baseline_fragment_rows": base_ghostish,
                    "note": "heuristic — verify visually before treating as a defect",
                }),
            )],
            confidence: 0.5,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}
