//! Screen diff engine (spec section 11/12/21). After every action we capture
//! BEFORE/AFTER and produce a comprehensive transition so Hermes can reason
//! about what changed without re-reading a full 160x50 grid.
//!
//! Fixes item 21: the diff now includes dimensions, cursor, and title changes,
//! not just cell text.
//!
//! Fixes item 14: `SemanticDiff` is now computed from the real
//! `semantic::analyze()` result instead of being a placeholder of zeros.
//! Control/region add/remove/change lists carry actual IDs so downstream
//! consumers (MCP, Hermes) can diff structures without re-analyzing.

use super::cell::{CursorState, ProcessState, ScreenState};
use crate::semantic;
use serde::{Deserialize, Serialize};

/// Change in terminal dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionChange {
    pub before_cols: u16,
    pub before_rows: u16,
    pub after_cols: u16,
    pub after_rows: u16,
}

/// Change in cursor position/visibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorChange {
    pub before: CursorState,
    pub after: CursorState,
}

/// Change in a terminal value (title, etc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueChange<T> {
    pub before: Option<T>,
    pub after: Option<T>,
}

/// Change in process state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessChange {
    pub before: ProcessState,
    pub after: ProcessState,
}

/// A span of text added or removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSpan {
    pub text: String,
    pub row: usize,
}

/// Comprehensive screen diff (spec section 21).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenDiff {
    /// Dimension change, if any.
    pub dimensions: Option<DimensionChange>,
    /// Number of cells whose visible content changed.
    pub changed_cells: usize,
    /// Cursor change, if any.
    pub cursor: Option<CursorChange>,
    /// Title change, if any.
    pub title: Option<ValueChange<String>>,
    /// Process state change, if any.
    pub process: Option<ProcessChange>,
    /// New text spans present after but not before.
    pub text_added: Vec<TextSpan>,
    /// Text spans present before but gone after.
    pub text_removed: Vec<TextSpan>,
    /// Number of cells with style-only changes (attributes changed, text same).
    pub style_changes: usize,
}

/// A per-control description of WHAT changed in a transition (finding 40:
/// semantic render diff — "which control the app draws differently and how",
/// not a raw cell count). One delta per changed control, with a human
/// summary ("button/save moved x:65→71") plus the resolved before/after
/// values for each changed property, so a caller can reason about render
/// drift without re-analyzing two frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRenderDelta {
    /// The control's stable semantic id (`button/save`, …).
    pub id: String,
    /// Human prose: what the control looks like before vs after.
    pub summary: String,
    /// Bounds delta when the control moved/resized, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BoundsDelta>,
    /// Label delta (renamed), else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<ValueChange<String>>,
    /// Value delta (text/field content), else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<ValueChange<String>>,
    /// Focus delta (gained/lost focus), else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    /// Selection delta, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    /// Checked delta, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// Enabled delta (grayed -> active or back), else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// How a control's bounds moved (finding 40): the `before`/`after` corners
/// plus a prose phrase encoding the motion (`moved x:65→71`, `resized
/// w:12→14`, `moved x:65→71 and resized h:3→4`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundsDelta {
    pub before: ControlBoundsSer,
    pub after: ControlBoundsSer,
    /// Human phrase, e.g. `"moved x:65→71"` or `"resized h:3→4"`.
    pub motion: String,
    /// Stable flag: did the control actually move/resize (vs true copy).
    pub geo_changed: bool,
}

/// Serializable mirror of [`crate::semantic::ControlBounds`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBoundsSer {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Semantic diff: changes in regions, controls, and focus.
///
/// Audit item 14: expanded from numeric counts (all-zero placeholder) to
/// full ID lists plus counts.  Old serialized data without the new Vec
/// fields still deserializes because they have `#[serde(default)]`.
///
/// Finding 40: adds `control_deltas` — a per-control `ControlRenderDelta`
/// describing WHAT changed (moved/resized/relabeled/re-focused), so a
/// render change reads as "button/save moved x:65→71", never just
/// `changed_cells: 47`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDiff {
    /// Control IDs added in the transition.
    #[serde(default)]
    pub controls_added: Vec<String>,
    /// Control IDs removed in the transition.
    #[serde(default)]
    pub controls_removed: Vec<String>,
    /// Number of changed controls (count retained for backward compatibility).
    pub controls_changed_count: usize,
    /// Control IDs that changed.
    #[serde(default)]
    pub controls_changed: Vec<String>,
    /// Per-control render deltas for the changed controls (finding 40).
    #[serde(default)]
    pub control_deltas: Vec<ControlRenderDelta>,
    /// Region IDs added in the transition.
    #[serde(default)]
    pub regions_added: Vec<String>,
    /// Region IDs removed in the transition.
    #[serde(default)]
    pub regions_removed: Vec<String>,
    /// Region IDs that changed.
    #[serde(default)]
    pub regions_changed: Vec<String>,
    /// Label of the focused control before the transition.
    pub focus_before: Option<String>,
    /// Label of the focused control after the transition.
    pub focus_after: Option<String>,
    /// Kept for backward compatibility -- number of controls added.
    #[serde(default)]
    pub controls_added_count: usize,
    /// Kept for backward compatibility -- number of controls removed.
    #[serde(default)]
    pub controls_removed_count: usize,
    /// Kept for backward compatibility -- number of regions added.
    #[serde(default)]
    pub regions_added_count: usize,
    /// Kept for backward compatibility -- number of regions removed.
    #[serde(default)]
    pub regions_removed_count: usize,
}

/// Full transition between two screens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    /// Structural hash before / after.
    pub before_structure_hash: String,
    pub after_structure_hash: String,
    pub before_visual_hash: String,
    pub after_visual_hash: String,
    /// Detailed screen diff.
    pub screen_diff: ScreenDiff,
    /// Semantic diff.
    pub semantic_diff: SemanticDiff,
}

/// Compute a transition between two screens.
pub fn diff(before: &ScreenState, after: &ScreenState) -> Transition {
    let screen_diff = compute_screen_diff(before, after);
    let semantic_diff = compute_semantic_diff(before, after);

    Transition {
        before_structure_hash: before.structure_hash.clone(),
        after_structure_hash: after.structure_hash.clone(),
        before_visual_hash: before.visual_hash.clone(),
        after_visual_hash: after.visual_hash.clone(),
        screen_diff,
        semantic_diff,
    }
}

// ── Screen diff ───────────────────────────────────────────────────────

/// Compute screen diff with dimensions, cursor, title, and process.
fn compute_screen_diff(before: &ScreenState, after: &ScreenState) -> ScreenDiff {
    let mut changed_cells = 0usize;
    let mut style_changes = 0usize;
    let n = before.cells.len().min(after.cells.len());

    for i in 0..n {
        let a = &before.cells[i];
        let b = &after.cells[i];
        if a.text != b.text {
            changed_cells += 1;
        } else if a.fg != b.fg
            || a.bg != b.bg
            || a.bold != b.bold
            || a.underline != b.underline
            || a.reverse != b.reverse
        {
            style_changes += 1;
        }
    }

    // If dimensions changed, count extra cells as changed
    if before.cells.len() != after.cells.len() {
        changed_cells += before.cells.len().abs_diff(after.cells.len());
    }

    let dimensions = if before.cols != after.cols || before.rows != after.rows {
        Some(DimensionChange {
            before_cols: before.cols,
            before_rows: before.rows,
            after_cols: after.cols,
            after_rows: after.rows,
        })
    } else {
        None
    };

    let cursor = if before.cursor.x != after.cursor.x
        || before.cursor.y != after.cursor.y
        || before.cursor.visible != after.cursor.visible
    {
        Some(CursorChange {
            before: before.cursor.clone(),
            after: after.cursor.clone(),
        })
    } else {
        None
    };

    let title = if before.title != after.title {
        Some(ValueChange {
            before: before.title.clone(),
            after: after.title.clone(),
        })
    } else {
        None
    };

    let process = if before.process.running != after.process.running
        || before.process.exit_code != after.process.exit_code
        || before.process.exit_signal != after.process.exit_signal
    {
        Some(ProcessChange {
            before: before.process.clone(),
            after: after.process.clone(),
        })
    } else {
        None
    };

    let text_added = collect_text_diffs(&after.viewport_text, &before.viewport_text);
    let text_removed = collect_text_diffs(&before.viewport_text, &after.viewport_text);

    ScreenDiff {
        dimensions,
        changed_cells,
        cursor,
        title,
        process,
        text_added,
        text_removed,
        style_changes,
    }
}

/// Collect text spans present in `to` but not in `from`.
fn collect_text_diffs(from: &[String], to: &[String]) -> Vec<TextSpan> {
    let from_joined = from.join("\n");
    let mut result = Vec::new();
    for (row_idx, row) in to.iter().enumerate() {
        for tok in row.split_whitespace().filter(|t| t.len() >= 2) {
            if !from_joined.contains(tok) && !result.iter().any(|s: &TextSpan| s.text == tok) {
                result.push(TextSpan {
                    text: tok.to_string(),
                    row: row_idx,
                });
            }
        }
    }
    result.truncate(40);
    result
}

// ── Semantic diff ─────────────────────────────────────────────────────

/// Compute the semantic diff between two screens.
///
/// Analyzes both screens, then compares the resulting semantic models
/// to produce add/remove/change lists for controls and regions.
fn compute_semantic_diff(before: &ScreenState, after: &ScreenState) -> SemanticDiff {
    let before_sem = semantic::analyze(before);
    let after_sem = semantic::analyze(after);

    // Build ID maps.
    let before_controls: std::collections::HashMap<&str, &semantic::Control> = before_sem
        .controls
        .iter()
        .map(|c| (c.id.as_str(), c))
        .collect();
    let after_controls: std::collections::HashMap<&str, &semantic::Control> = after_sem
        .controls
        .iter()
        .map(|c| (c.id.as_str(), c))
        .collect();

    let before_regions: std::collections::HashMap<&str, &semantic::Region> = before_sem
        .regions
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();
    let after_regions: std::collections::HashMap<&str, &semantic::Region> = after_sem
        .regions
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();

    // Controls: added / removed / changed.
    let mut controls_added = Vec::new();
    let mut controls_removed = Vec::new();
    let mut controls_changed = Vec::new();

    for id in after_controls.keys() {
        if !before_controls.contains_key(*id) {
            controls_added.push(id.to_string());
        }
    }
    let mut control_deltas: Vec<ControlRenderDelta> = Vec::new();
    for (id, before_ctrl) in &before_controls {
        if !after_controls.contains_key(id) {
            controls_removed.push(id.to_string());
        } else {
            let after_ctrl = after_controls.get(id).unwrap();
            if control_properties_differ(before_ctrl, after_ctrl) {
                controls_changed.push(id.to_string());
                // Finding 40: only emit a delta when the control's rendered
                // state actually changed — the delta is the semantic
                // description of HOW it changed, not a phantom no-op row.
                if let Some(delta) = control_render_delta(before_ctrl, after_ctrl) {
                    control_deltas.push(delta);
                }
            }
        }
    }

    // Regions: added / removed / changed.
    let mut regions_added = Vec::new();
    let mut regions_removed = Vec::new();
    let mut regions_changed = Vec::new();

    for id in after_regions.keys() {
        if !before_regions.contains_key(*id) {
            regions_added.push(id.to_string());
        }
    }
    for (id, before_reg) in &before_regions {
        if !after_regions.contains_key(id) {
            regions_removed.push(id.to_string());
        } else {
            let after_reg = after_regions.get(id).unwrap();
            if region_properties_differ(before_reg, after_reg) {
                regions_changed.push(id.to_string());
            }
        }
    }

    // Focus.
    let focus_before = before_sem.focus.control.clone();
    let focus_after = after_sem.focus.control.clone();

    // Backward-compatible counts (compute before moving the Vecs).
    let controls_added_count = controls_added.len();
    let controls_removed_count = controls_removed.len();
    let regions_added_count = regions_added.len();
    let regions_removed_count = regions_removed.len();

    SemanticDiff {
        controls_added,
        controls_removed,
        controls_changed_count: controls_changed.len(),
        controls_changed,
        control_deltas,
        regions_added,
        regions_removed,
        regions_changed,
        focus_before,
        focus_after,
        controls_added_count,
        controls_removed_count,
        regions_added_count,
        regions_removed_count,
    }
}

/// Compare two controls for differences in observable properties.
fn control_properties_differ(a: &semantic::Control, b: &semantic::Control) -> bool {
    a.kind != b.kind
        || a.label != b.label
        || a.value != b.value
        || a.checked != b.checked
        || a.selected != b.selected
        || a.focused != b.focused
        || a.enabled != b.enabled
}

/// Build a per-control render delta (finding 40) for a control that exists
/// in both frames but whose rendered state differs. Returns `None` for a
/// control whose differing property is not a *render* concern (or when the
/// two sides are identical — a guard for drift). The delta carries a prose
/// summary plus the resolved before/after for each changed property.
fn control_render_delta(
    before: &crate::semantic::Control,
    after: &crate::semantic::Control,
) -> Option<ControlRenderDelta> {
    // Bounds delta.
    let bounds_changed = before.bounds.x != after.bounds.x
        || before.bounds.y != after.bounds.y
        || before.bounds.width != after.bounds.width
        || before.bounds.height != after.bounds.height;
    let bounds = if bounds_changed {
        Some(BoundsDelta {
            before: ControlBoundsSer {
                x: before.bounds.x,
                y: before.bounds.y,
                width: before.bounds.width,
                height: before.bounds.height,
            },
            after: ControlBoundsSer {
                x: after.bounds.x,
                y: after.bounds.y,
                width: after.bounds.width,
                height: after.bounds.height,
            },
            motion: bounds_motion_phrase(&before.bounds, &after.bounds),
            geo_changed: true,
        })
    } else {
        None
    };

    let label = if before.label != after.label {
        Some(ValueChange {
            before: Some(before.label.clone()),
            after: Some(after.label.clone()),
        })
    } else {
        None
    };
    let value = if before.value != after.value {
        Some(ValueChange {
            before: before.value.clone(),
            after: after.value.clone(),
        })
    } else {
        None
    };
    let focused = if before.focused != after.focused {
        Some(after.focused)
    } else {
        None
    };
    let selected = if before.selected != after.selected {
        Some(after.selected)
    } else {
        None
    };
    let checked = if before.checked != after.checked {
        Some(after.checked)
    } else {
        None
    };
    let enabled = if before.enabled != after.enabled {
        Some(after.enabled)
    } else {
        None
    };

    // If nothing is a render hazard, return None (the diff loop already
    // gated on control_properties_differ, so a changed control always has
    // at least one differing field; this is defense against drift).
    if bounds.is_none()
        && label.is_none()
        && value.is_none()
        && focused.is_none()
        && selected.is_none()
        && checked.is_none()
        && enabled.is_none()
    {
        return None;
    }

    // Human summary: "button/save moved x:65→71", "button/save label
    // 'Save'→'Save All'", "button/save gained focus", … Concatenate the
    // non-trivial clauses.
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = &bounds {
        parts.push(b.motion.clone());
    }
    if let Some(l) = &label {
        parts.push(format!(
            "label {:?} → {:?}",
            l.before.as_deref().unwrap_or(""),
            l.after.as_deref().unwrap_or("")
        ));
    }
    if let Some(v) = &value {
        parts.push(format!(
            "value {:?} → {:?}",
            v.before.as_deref().unwrap_or(""),
            v.after.as_deref().unwrap_or("")
        ));
    }
    if focused.is_some() {
        parts.push(if after.focused {
            "gained focus".to_string()
        } else {
            "lost focus".to_string()
        });
    }
    if selected.is_some() {
        parts.push(if after.selected {
            "selected".to_string()
        } else {
            "deselected".to_string()
        });
    }
    if checked.is_some() {
        parts.push(if after.checked {
            "checked".to_string()
        } else {
            "unchecked".to_string()
        });
    }
    if enabled.is_some() {
        parts.push(if after.enabled {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        });
    }
    // Dedup identical clauses (motion may repeat "moved x…" form) and join.
    let mut seen = std::collections::HashSet::new();
    parts.retain(|p| seen.insert(p.clone()));
    let summary = format!("{} {}", after.id, parts.join(", "));
    Some(ControlRenderDelta {
        id: after.id.clone(),
        summary,
        bounds,
        label,
        value,
        focused,
        selected,
        checked,
        enabled,
    })
}

/// Human phrase encoding how bounds moved (finding 40): "moved x:65→71",
/// "resized w:12→14", or a combined clause. Only names dimensions that
/// actually changed, so a pure resize never reads as a move.
fn bounds_motion_phrase(
    before: &crate::semantic::ControlBounds,
    after: &crate::semantic::ControlBounds,
) -> String {
    let moved = before.x != after.x || before.y != after.y;
    let resized = before.width != after.width || before.height != after.height;
    let mut clauses = Vec::new();
    if moved {
        let mut dims = Vec::new();
        if before.x != after.x {
            dims.push(format!("x:{}→{}", before.x, after.x));
        }
        if before.y != after.y {
            dims.push(format!("y:{}→{}", before.y, after.y));
        }
        clauses.push(format!("moved {}", dims.join(" ")));
    }
    if resized {
        let mut dims = Vec::new();
        if before.width != after.width {
            dims.push(format!("w:{}→{}", before.width, after.width));
        }
        if before.height != after.height {
            dims.push(format!("h:{}→{}", before.height, after.height));
        }
        clauses.push(format!("resized {}", dims.join(" ")));
    }
    if clauses.is_empty() {
        String::new()
    } else {
        clauses.join(" and ")
    }
}

/// Compare two regions for differences in observable properties.
fn region_properties_differ(a: &semantic::Region, b: &semantic::Region) -> bool {
    a.kind != b.kind
        || a.title != b.title
        || a.clipping_state != b.clipping_state
        || a.bounds.x != b.bounds.x
        || a.bounds.y != b.bounds.y
        || a.bounds.width != b.bounds.width
        || a.bounds.height != b.bounds.height
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::cell::Cell;
    use crate::screen::cell::Color;
    use crate::screen::cell::ProcessState;

    fn make_screen(cols: u16, rows: u16, text: Vec<&str>) -> ScreenState {
        let mut cells = Vec::new();
        let mut viewport_text = Vec::new();
        for (y, line) in text.iter().enumerate() {
            viewport_text.push(line.to_string());
            for (x, ch) in line.chars().enumerate() {
                cells.push(Cell {
                    x: x as u16,
                    y: y as u16,
                    text: ch.to_string(),
                    fg: Color::unknown(),
                    bg: Color::unknown(),
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                    strike: false,
                });
            }
        }
        ScreenState {
            cols,
            rows,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells,
            viewport_text,
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn test_diff_empty_screens() {
        let a = make_screen(80, 24, vec![""; 24]);
        let b = make_screen(80, 24, vec![""; 24]);
        let t = diff(&a, &b);
        assert_eq!(t.screen_diff.changed_cells, 0);
        assert!(t.screen_diff.dimensions.is_none());
        assert!(t.screen_diff.cursor.is_none());
    }

    #[test]
    fn test_diff_structure_hash_preserved() {
        let a = make_screen(80, 24, vec!["Hello"; 24]);
        let b = make_screen(80, 24, vec!["Hello"; 24]);
        let t = diff(&a, &b);
        assert_eq!(t.before_structure_hash, a.structure_hash);
        assert_eq!(t.after_structure_hash, b.structure_hash);
    }

    // ── finding 40: semantic render deltas (per-control WHAT changed) ──

    fn ctrl(id: &str, x: u16, y: u16, w: u16, h: u16, focused: bool) -> crate::semantic::Control {
        crate::semantic::Control {
            id: id.to_string(),
            kind: crate::semantic::ControlKind::Button,
            label: id.to_string(),
            value: None,
            bounds: crate::semantic::ControlBounds {
                x,
                y,
                width: w,
                height: h,
            },
            region_id: None,
            focusable: true,
            focused,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: crate::semantic::Confidence::inferred(0.9, &[]),
            evidence: Vec::new(),
            source: "inferred".to_string(),
        }
    }

    #[test]
    fn render_delta_describes_moved_control() {
        // "button/save moved x:65→71" — the finding 40 headline.
        let before = ctrl("button/save", 65, 3, 8, 1, false);
        let after = ctrl("button/save", 71, 3, 8, 1, false);
        let d = control_render_delta(&before, &after).expect("delta");
        assert_eq!(d.id, "button/save");
        assert!(d.summary.contains("moved x:65\u{2192}71"), "{}", d.summary);
        let b = d.bounds.as_ref().expect("bounds delta");
        assert_eq!((b.before.x, b.after.x), (65, 71));
        assert!(d.label.is_none() && d.focused.is_none(), "pure move");
    }

    #[test]
    fn render_delta_describes_focus_and_resize() {
        // Focus gain + resize in one transition → two clauses.
        let before = ctrl("button/beta", 10, 5, 12, 1, false);
        let after = ctrl("button/beta", 10, 5, 14, 1, true);
        let d = control_render_delta(&before, &after).expect("delta");
        assert!(d.summary.contains("gained focus"), "{}", d.summary);
        assert!(d.summary.contains("resized w:12→14"), "{}", d.summary);
        assert_eq!(d.focused, Some(true));
        let b = d.bounds.as_ref().expect("bounds");
        assert_eq!((b.before.width, b.after.width), (12, 14));
    }

    #[test]
    fn identical_controls_produce_no_delta() {
        let a = ctrl("button/save", 3, 3, 8, 1, false);
        let b = ctrl("button/save", 3, 3, 8, 1, false);
        assert!(
            control_render_delta(&a, &b).is_none(),
            "no delta for identical"
        );
    }
}
