//! Screen diff engine (spec section 11/12/21). After every action we capture
//! BEFORE/AFTER and produce a comprehensive transition so Hermes can reason
//! about what changed without re-reading a full 160x50 grid.
//!
//! Fixes item 21: the diff now includes dimensions, cursor, and title changes,
//! not just cell text.

use super::cell::{CursorState, ProcessState, ScreenState};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub controls_added: usize,
    pub controls_removed: usize,
    pub regions_added: usize,
    pub regions_removed: usize,
    pub focus_before: Option<String>,
    pub focus_after: Option<String>,
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
    let semantic_diff = SemanticDiff {
        controls_added: 0,
        controls_removed: 0,
        regions_added: 0,
        regions_removed: 0,
        focus_before: None,
        focus_after: None,
    };

    Transition {
        before_structure_hash: before.structure_hash.clone(),
        after_structure_hash: after.structure_hash.clone(),
        before_visual_hash: before.visual_hash.clone(),
        after_visual_hash: after.visual_hash.clone(),
        screen_diff,
        semantic_diff,
    }
}

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
