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

/// Semantic diff: changes in regions, controls, and focus.
///
/// Audit item 14: expanded from numeric counts (all-zero placeholder) to
/// full ID lists plus counts.  Old serialized data without the new Vec
/// fields still deserializes because they have `#[serde(default)]`.
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
    for (id, before_ctrl) in &before_controls {
        if !after_controls.contains_key(id) {
            controls_removed.push(id.to_string());
        } else {
            let after_ctrl = after_controls.get(id).unwrap();
            if control_properties_differ(before_ctrl, after_ctrl) {
                controls_changed.push(id.to_string());
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
}
