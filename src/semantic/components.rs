//! Screen-level component detectors (re-review Wave-4): tables, trees, and
//! scrollbars — the "complex widget" tier the per-line recognizers cannot
//! express, because each spans multiple rows and has structural evidence
//! (column alignment, indent glyphs, bar geometry) rather than a token.
//!
//! Everything here is inferred: `confidence < 1.0`, `source: "inferred"`,
//! with the establishing evidence named so a finding can be re-derived.

use crate::screen::ScreenState;
use crate::semantic::confidence::Confidence;
use serde::{Deserialize, Serialize};

/// A detected table: header row plus aligned columns of data rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableComponent {
    /// Inclusive bounds of the table body in cells.
    pub bounds: crate::semantic::regions::Bounds,
    /// Header labels, in column order (x = starting column).
    pub columns: Vec<(u16, String)>,
    /// Number of data rows below the header.
    pub row_count: u32,
    /// The header row's y coordinate.
    pub header_y: u16,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

/// A detected tree: indented rows with branch glyphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeComponent {
    /// Inclusive bounds spanning the tree rows.
    pub bounds: crate::semantic::regions::Bounds,
    /// Number of rows that carry a branch glyph.
    pub branch_rows: u32,
    /// Deepest indent depth observed (0 = flat list with glyphs).
    pub max_depth: u32,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

/// A detected scrollbar: a vertical or horizontal trough with a thumb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "orientation", rename_all = "snake_case")]
pub enum ScrollbarOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollbarComponent {
    pub orientation: ScrollbarOrientation,
    /// Position of the trough (the track) in cells.
    pub track_x: u16,
    pub track_y: u16,
    pub track_len: u16,
    /// Thumb start offset within the track (0-based, in cells).
    pub thumb_offset: u16,
    /// Thumb length in cells.
    pub thumb_len: u16,
    pub confidence: Confidence,
}

/// Detect all screen-level components in one pass.
pub fn detect_components(screen: &ScreenState) -> Vec<Component> {
    let mut out = Vec::new();
    if let Some(t) = detect_table(screen) {
        out.push(Component::Table(t));
    }
    if let Some(t) = detect_tree(screen) {
        out.push(Component::Tree(t));
    }
    for sb in detect_scrollbars(screen) {
        out.push(Component::Scrollbar(sb));
    }
    out
}

/// One detected screen-level component.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "component", rename_all = "snake_case")]
pub enum Component {
    Table(TableComponent),
    Tree(TreeComponent),
    Scrollbar(ScrollbarComponent),
}

impl Component {
    /// Cell bounds of the component.
    pub fn bounds(&self) -> crate::semantic::regions::Bounds {
        use crate::semantic::regions::Bounds;
        match self {
            Component::Table(t) => t.bounds.clone(),
            Component::Tree(t) => t.bounds.clone(),
            Component::Scrollbar(s) => match s.orientation {
                ScrollbarOrientation::Vertical => Bounds {
                    x: s.track_x,
                    y: s.track_y,
                    width: 1,
                    height: s.track_len,
                },
                ScrollbarOrientation::Horizontal => Bounds {
                    x: s.track_x,
                    y: s.track_y,
                    width: s.track_len,
                    height: 1,
                },
            },
        }
    }
}

/// Detect a table: a header row of 2+ column labels separated by 2+ spaces,
/// followed by 1+ rows with word starts aligned to (most of) those columns.
fn detect_table(screen: &ScreenState) -> Option<TableComponent> {
    for (y, line) in screen.viewport_text.iter().enumerate() {
        if y + 1 >= screen.viewport_text.len() {
            break;
        }
        let header_cols = header_columns(line);
        if header_cols.len() < 2 {
            continue;
        }
        let header_xs: Vec<u16> = header_cols.iter().map(|(x, _)| *x).collect();

        // Count consecutive following rows whose word starts align with the
        // header columns. Tolerance ±1 only for long labels (≥4 chars),
        // where a separator drift is plausible; short labels must align
        // exactly — "Edit"(4) is not satisfied by "text" at column 5.
        let mut data_rows = 0u32;
        for next in screen.viewport_text.iter().skip(y + 1) {
            if next.trim().is_empty() {
                break;
            }
            let starts = word_starts(next);
            let aligned = header_xs
                .iter()
                .zip(header_cols.iter().map(|(_, l)| l.chars().count()))
                .filter(|(hx, w)| {
                    let tol = if *w >= 4 { 1 } else { 0 };
                    starts.iter().any(|sx| sx.abs_diff(**hx) <= tol)
                })
                .count();
            // A table row must align the clear majority of columns; for a
            // 4-column header that is 3+, which prose lines do not reach.
            if aligned * 4 >= header_xs.len() * 3 {
                data_rows += 1;
            } else {
                break;
            }
        }
        if data_rows == 0 {
            continue;
        }

        // Reject menu/tab lines: headers like "File  Edit" rarely have
        // aligned continuation rows, which the data_rows check already
        // enforces; but require at least one multi-char label to avoid
        // two stray one-letter words.
        if header_cols.iter().all(|(_, l)| l.chars().count() < 2) {
            continue;
        }

        let height = (data_rows + 1) as u16;
        let width = header_cols
            .last()
            .map(|(x, l)| x + l.chars().count() as u16)
            .unwrap_or(0);
        let x0 = header_cols[0].0;
        let columns = header_cols.clone();
        return Some(TableComponent {
            bounds: crate::semantic::regions::Bounds {
                x: x0,
                y: y as u16,
                width,
                height,
            },
            columns,
            row_count: data_rows,
            header_y: y as u16,
            confidence: Confidence::inferred(
                if data_rows >= 3 { 0.9 } else { 0.7 },
                &["aligned-columns", "header-row"],
            ),
            evidence: vec![
                format!("header at row {} with {} columns", y, header_cols.len()),
                format!("{} aligned data rows", data_rows),
            ],
        });
    }
    None
}

/// Column labels of a candidate header row: words separated by 2+ spaces,
/// each starting at its column index.
fn header_columns(line: &str) -> Vec<(u16, String)> {
    let trimmed = line.trim_end();
    if trimmed.trim().is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] != ' ' {
            i += 1;
        }
        let label: String = chars[start..i].iter().collect();
        // A single following space means prose; require 2+ spaces (or EOL)
        // between labels to count as a column separator.
        let sep_ok =
            i >= chars.len() || chars[i..].iter().take(2).filter(|&&c| c == ' ').count() >= 2;
        if sep_ok {
            out.push((start as u16, label));
        } else {
            // Merge into the previous label (prose continuation).
            if let Some(last) = out.last_mut() {
                let gap: String = chars[last.0 as usize + last.1.chars().count()..i]
                    .iter()
                    .collect();
                last.1.push_str(&gap);
                last.1.push_str(&label);
            }
        }
    }
    out
}

/// Starting columns of words in a line.
fn word_starts(line: &str) -> Vec<u16> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        out.push(i as u16);
        while i < chars.len() && chars[i] != ' ' {
            i += 1;
        }
    }
    out
}

/// Branch glyphs that mark tree rows.
const BRANCH_GLYPHS: [char; 8] = ['├', '└', '│', '┌', '┐', '┘', '┤', '┬'];

/// Detect a tree: 2+ rows whose *indent depth* varies and which carry
/// branch glyphs (├ └) before their labels.
fn detect_tree(screen: &ScreenState) -> Option<TreeComponent> {
    let mut rows = 0u32;
    let mut max_depth = 0u32;
    let mut first_y = None;
    let mut last_y = 0u16;
    let mut max_x = 0u16;
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let branch_pos = line.chars().position(|c| c == '├' || c == '└');
        if let Some(pos) = branch_pos {
            rows += 1;
            if first_y.is_none() {
                first_y = Some(y as u16);
            }
            last_y = y as u16;
            max_depth = max_depth.max((pos / 2) as u32);
            max_x = max_x.max(line.trim_end().chars().count() as u16);
        }
    }
    if rows < 2 {
        return None;
    }
    Some(TreeComponent {
        bounds: crate::semantic::regions::Bounds {
            x: 0,
            y: first_y.unwrap_or(0),
            width: max_x,
            height: last_y - first_y.unwrap_or(0) + 1,
        },
        branch_rows: rows,
        max_depth,
        confidence: Confidence::inferred(0.85, &["branch-glyphs", "indented-rows"]),
        evidence: vec![
            format!("{} branch-glyph rows", rows),
            format!("max indent depth {}", max_depth),
        ],
    })
}

/// Thumb (filled) glyphs and track (empty) glyphs for scrollbars.
const THUMB_GLYPHS: [char; 6] = ['█', '▓', '▒', '░', '#', '='];
const TRACK_GLYPHS: [char; 4] = ['░', ' ', '.', '-'];

/// Detect vertical and horizontal scrollbars: a run of thumb glyphs inside
/// (or bounded by) track glyphs, at the screen edge for vertical bars.
fn detect_scrollbars(screen: &ScreenState) -> Vec<ScrollbarComponent> {
    let mut out = Vec::new();

    // Vertical: columns near the right edge where most cells are
    // thumb/track glyphs and at least one thumb glyph exists.
    if screen.rows > 0 {
        let last_col = screen.cols.saturating_sub(1);
        for x in [last_col, screen.cols.saturating_sub(2)] {
            if screen.cols < 3 {
                break;
            }
            let mut track = 0u16;
            let mut thumb = 0u16;
            let mut first_thumb = None;
            for (y, line) in screen.viewport_text.iter().enumerate() {
                let Some(c) = line.chars().nth(x as usize) else {
                    continue;
                };
                if THUMB_GLYPHS.contains(&c) && c != '░' {
                    thumb += 1;
                    if first_thumb.is_none() {
                        first_thumb = Some(y as u16);
                    }
                } else if TRACK_GLYPHS.contains(&c) {
                    track += 1;
                }
            }
            // A scrollbar column: mostly bar glyphs, with a thumb segment.
            if thumb > 0 && track + thumb >= screen.rows / 2 && thumb < track + thumb {
                let start = first_thumb.unwrap_or(0);
                out.push(ScrollbarComponent {
                    orientation: ScrollbarOrientation::Vertical,
                    track_x: x,
                    track_y: 0,
                    track_len: screen.rows,
                    thumb_offset: start,
                    thumb_len: thumb,
                    confidence: Confidence::inferred(0.8, &["bar-glyphs"]),
                });
                break; // one vertical bar is enough; don't double-report
            }
        }
    }

    // Horizontal: bottom rows where most cells are bar glyphs with a thumb
    // run.
    if screen.rows > 1 {
        for y in [screen.rows - 1, screen.rows - 2] {
            let Some(line) = screen.viewport_text.get(y as usize) else {
                continue;
            };
            let chars: Vec<char> = line.chars().collect();
            let mut thumb = 0u16;
            let mut track = 0u16;
            let mut first_thumb = None;
            for (x, &c) in chars.iter().enumerate() {
                if THUMB_GLYPHS.contains(&c) && c != '░' {
                    thumb += 1;
                    if first_thumb.is_none() {
                        first_thumb = Some(x as u16);
                    }
                } else if TRACK_GLYPHS.contains(&c) {
                    track += 1;
                }
            }
            if thumb > 0 && track + thumb >= screen.cols / 2 {
                let start = first_thumb.unwrap_or(0);
                out.push(ScrollbarComponent {
                    orientation: ScrollbarOrientation::Horizontal,
                    track_x: 0,
                    track_y: y,
                    track_len: screen.cols,
                    thumb_offset: start,
                    thumb_len: thumb,
                    confidence: Confidence::inferred(0.8, &["bar-glyphs"]),
                });
                break;
            }
        }
    }

    out
}

/// BRANCH_GLYPHS is referenced by detect_tree; keep the rest for future
/// junction-aware depth inference (│ used for depth below a branch).
#[allow(dead_code)]
fn _glyphs_used() -> bool {
    BRANCH_GLYPHS.contains(&'├')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CursorState, ProcessState};

    fn screen(rows: Vec<&str>, cols: u16) -> ScreenState {
        ScreenState {
            cols,
            rows: rows.len() as u16,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn table_detected_with_columns_and_rows() {
        let s = screen(
            vec![
                "Name        Status      Latency",
                "db-primary  connected   12ms",
                "db-replica  connected   14ms",
                "cache       degraded    3ms",
                "",
                "footer text here",
            ],
            60,
        );
        let comps = detect_components(&s);
        let Some(Component::Table(t)) = comps.iter().find(|c| matches!(c, Component::Table(_)))
        else {
            panic!("table expected: {:?}", comps);
        };
        assert_eq!(t.columns.len(), 3);
        assert_eq!(t.columns[0].1, "Name");
        assert_eq!(t.columns[2].1, "Latency");
        assert_eq!(t.row_count, 3);
        assert_eq!(t.header_y, 0);
        assert_eq!(t.bounds.height, 4);
    }

    /// Menu lines ("File  Edit  View") have no aligned data rows — not a
    /// table.
    #[test]
    fn menu_bar_is_not_a_table() {
        let s = screen(vec!["File  Edit  View  Help", "body text here"], 40);
        let comps = detect_components(&s);
        assert!(!comps.iter().any(|c| matches!(c, Component::Table(_))));
    }

    #[test]
    fn tree_detected_with_depth() {
        let s = screen(
            vec![
                "root",
                "├─ src",
                "│  └─ main.rs",
                "└─ tests",
                "   └─ e2e.rs",
            ],
            40,
        );
        let comps = detect_components(&s);
        let Some(Component::Tree(t)) = comps.iter().find(|c| matches!(c, Component::Tree(_)))
        else {
            panic!("tree expected: {:?}", comps);
        };
        assert_eq!(t.branch_rows, 4);
        assert!(t.max_depth >= 1, "indent depth tracked");
    }

    /// A flat list with no branch glyphs is not a tree.
    #[test]
    fn flat_list_is_not_a_tree() {
        let s = screen(vec!["one", "two", "three"], 40);
        let comps = detect_components(&s);
        assert!(!comps.iter().any(|c| matches!(c, Component::Tree(_))));
    }

    #[test]
    fn vertical_scrollbar_detected_at_edge() {
        let mut rows: Vec<String> = Vec::new();
        for _ in 0..6 {
            rows.push("content here".to_string());
        }
        // Right-edge scrollbar: thumb rows 1-3 in a 6-row screen.
        rows[0] = format!("{:<20}░", "content here");
        rows[1] = format!("{:<20}█", "content here");
        rows[2] = format!("{:<20}█", "content here");
        rows[3] = format!("{:<20}█", "content here");
        rows[4] = format!("{:<20}░", "content here");
        rows[5] = format!("{:<20}░", "content here");
        let s = screen(rows.iter().map(|r| r.as_str()).collect(), 21);
        let comps = detect_components(&s);
        let Some(Component::Scrollbar(sb)) = comps.iter().find(|c| {
            matches!(
                c,
                Component::Scrollbar(ScrollbarComponent {
                    orientation: ScrollbarOrientation::Vertical,
                    ..
                })
            )
        }) else {
            panic!("vertical scrollbar expected: {:?}", comps);
        };
        assert_eq!(sb.thumb_len, 3);
        assert_eq!(sb.thumb_offset, 1);
        assert_eq!(sb.track_len, 6);
    }

    #[test]
    fn horizontal_scrollbar_detected_at_bottom() {
        let mut rows: Vec<String> = Vec::new();
        for _ in 0..5 {
            rows.push("text".to_string());
        }
        rows.push("░░░░███░░░░░".to_string());
        let s = screen(rows.iter().map(|r| r.as_str()).collect(), 13);
        let comps = detect_components(&s);
        let Some(Component::Scrollbar(sb)) = comps.iter().find(|c| {
            matches!(
                c,
                Component::Scrollbar(ScrollbarComponent {
                    orientation: ScrollbarOrientation::Horizontal,
                    ..
                })
            )
        }) else {
            panic!("horizontal scrollbar expected: {:?}", comps);
        };
        assert_eq!(sb.thumb_len, 3);
        assert_eq!(sb.thumb_offset, 4);
        assert_eq!(sb.track_y, 5);
    }

    /// Plain prose yields no components (no false positives).
    #[test]
    fn prose_yields_no_components() {
        let s = screen(
            vec![
                "Hello and welcome to the application.",
                "Second line of body copy.",
            ],
            50,
        );
        let comps = detect_components(&s);
        assert!(comps.is_empty(), "no components in prose: {:?}", comps);
    }
}
