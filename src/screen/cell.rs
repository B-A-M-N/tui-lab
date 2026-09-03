//! Canonical cell + screen model (spec section 7).
//!
//! The raw terminal buffer is authoritative. We consume `vt100`'s parsed
//! grid rather than reconstructing styling from ANSI streams. The engine never
//! leaks backend types into the MCP contract.

use std::fmt::Write as _;

/// RGB color, or an unresolved palette slot (ANSI index) when the true color
/// is unknown. We deliberately keep `palette` so the color/style audit
/// (spec section 20) can say "contrast = unverifiable" instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Color {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rgb: Option<(u8, u8, u8)>,
    /// ANSI 0-255 index when only a palette slot is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<u8>,
}

impl Color {
    pub fn unknown() -> Self {
        Color {
            rgb: None,
            palette: None,
        }
    }
}

/// One terminal cell.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cell {
    pub x: u16,
    pub y: u16,
    /// grapheme text (may be empty for a space or a wide-char spacer).
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

impl Cell {
    pub fn is_blank(&self) -> bool {
        self.text.is_empty() || self.text == " "
    }
}

/// Cursor state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// Process / shell state tracked per session.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessState {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub pid: Option<u32>,
}

/// An OSC8 hyperlink observed on screen (Wave C item 29).
///
/// Spans are recorded in cell coordinates at open/close time (the vt grid
/// itself does not carry link state). Observation only — the URI is recorded,
/// never fetched.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Hyperlink {
    /// The `id=` parameter from the OSC8 params, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub uri: String,
    /// `(x, y)` where the link text starts.
    pub start: (u16, u16),
    /// `(x, y)` where it ends (exclusive); `None` when still open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<(u16, u16)>,
}

/// Full screen snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenState {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Row-major cells (length == cols*rows); sparse blanks omitted on output
    /// by the observation layer, but kept full here for diffing.
    pub cells: Vec<Cell>,
    /// Plain-text viewport, one String per row.
    pub viewport_text: Vec<String>,
    /// Scrollback lines (most recent last).
    #[serde(default)]
    pub scrollback: Vec<String>,
    /// OSC8 hyperlinks observed on this screen (item 29).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hyperlinks: Vec<Hyperlink>,
    pub raw_hash: String,
    pub visual_hash: String,
    pub structure_hash: String,
    pub process: ProcessState,
}

impl ScreenState {
    /// Create a new empty screen state with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        ScreenState {
            cols,
            rows,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: Vec::new(),
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

    /// Compact text view (used by `summary` observation).
    pub fn text_view(&self) -> String {
        let mut out = String::new();
        for row in &self.viewport_text {
            let _ = writeln!(out, "{}", row);
        }
        out
    }

    /// Re-review P0 (real SemanticChange): a stable identity over the
    /// semantic content of this frame — controls, regions, affordances,
    /// focus. Same identity ⇒ same semantic truth regardless of volatile
    /// pixels (spinner, clock); different identity ⇒ genuine semantic
    /// change. Delegates to [`crate::semantic::semantic_identity`].
    pub fn semantic_identity(&self) -> String {
        crate::semantic::semantic_identity(self)
    }

    /// The cells of one viewport row, in column order.
    fn row_cells(&self, y: u16) -> Vec<&Cell> {
        let mut row: Vec<&Cell> = self.cells.iter().filter(|c| c.y == y).collect();
        row.sort_by_key(|c| c.x);
        row
    }

    /// Review P0.6: does `text` render at terminal column `x`, row `y`?
    ///
    /// Coordinates are CELLS, never byte offsets: the row's cells are
    /// addressed through [`CellString`] (wide CJK/emoji glyphs own their
    /// continuation column, combining marks ride their base cell), so a
    /// match against `界` at column 10 or `🙂` at the last column is a
    /// column question with a column answer. A text whose glyphs need more
    /// columns than remain on the row cannot match. Never panics: any
    /// coordinate is in bounds or simply misses.
    pub fn match_text_at(&self, x: u16, y: u16, text: &str) -> bool {
        if text.is_empty() {
            return true;
        }
        let want = crate::screen::cell_string::CellString::from_text(text);
        let need = want.display_width();
        // Past the row's right edge by geometry: cannot fit.
        if x.saturating_add(need) > self.cols {
            return false;
        }
        let Some(row) = self.row_text_cells(y) else {
            return false;
        };
        // Compare glyph by glyph in column space. Every slot of `want` must
        // equal the screen slot at its column (wide glyphs compared whole,
        // so a match can never start mid-glyph). A column with no cell at
        // all (sparse frame) reads as blank.
        for slot in want.slots() {
            let at = x.saturating_add(slot.col);
            let have = match row.glyph_at(at) {
                Some(g) => g.text.as_str(),
                None => " ",
            };
            if have != slot.text {
                return false;
            }
        }
        true
    }

    /// The row at `y` as a column-addressed [`CellString`], or `None` when
    /// the row is out of range or the frame carries no text.
    ///
    /// Sources, in preference order: the parsed `cells` (authoritative, wide
    /// and combining glyphs addressed by column), then the `viewport_text`
    /// row (the plain-text projection for frames that carry no cell grid —
    /// matched through the same column arithmetic).
    fn row_text_cells(&self, y: u16) -> Option<crate::screen::cell_string::CellString> {
        if y >= self.rows {
            return None;
        }
        if !self.cells.is_empty() {
            let cells: Vec<(u16, &str)> = self
                .row_cells(y)
                .into_iter()
                .map(|c| (c.x, c.text.as_str()))
                .collect();
            if cells.is_empty() {
                return None;
            }
            return Some(crate::screen::cell_string::CellString::from_cells(
                cells, self.cols,
            ));
        }
        // No cell grid: fall back to the viewport row, one char per column.
        let text = self.viewport_text.get(y as usize)?.as_str();
        Some(crate::screen::cell_string::CellString::from_text(text))
    }
}

/// Review P0.6 release matrix: position matching in terminal cells must be
/// correct for every glyph class a real TUI can emit, and must never panic.
#[cfg(test)]
mod match_text_at_tests {
    use super::{Cell, Color, CursorState, ScreenState};

    /// A screen whose rows are filled from `text`, one grapheme per cell
    /// (wide glyphs occupy their continuation column as an empty cell).
    fn screen_with_row(cols: u16, row_contents: &[&str]) -> ScreenState {
        let mut s = ScreenState::new(cols, 4);
        let mut x = 0u16;
        for g in row_contents {
            let w = unicode_width::UnicodeWidthStr::width(*g).max(1) as u16;
            s.cells.push(Cell {
                x,
                y: 1,
                text: (*g).to_string(),
                fg: Color::unknown(),
                bg: Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            });
            // Continuation cells for a wide glyph (empty text, owned column).
            for cont in 1..w {
                s.cells.push(Cell {
                    x: x + cont,
                    y: 1,
                    text: String::new(),
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
            x += w;
        }
        // Blank-fill the rest of the row so glyph_at sees dense slots.
        while x < cols {
            s.cells.push(Cell {
                x,
                y: 1,
                text: " ".to_string(),
                fg: Color::unknown(),
                bg: Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            });
            x += 1;
        }
        s
    }

    fn cell(x: u16, y: u16, t: &str) -> Cell {
        Cell {
            x,
            y,
            text: t.to_string(),
            fg: Color::unknown(),
            bg: Color::unknown(),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strike: false,
        }
    }

    #[test]
    fn ascii_matches_by_column() {
        // "hello world" at row 1.
        let glyphs: Vec<&str> = "hello world".split("").filter(|s| !s.is_empty()).collect();
        let s = screen_with_row(20, &glyphs);
        assert!(s.match_text_at(0, 1, "hello"));
        assert!(s.match_text_at(6, 1, "world"));
        assert!(!s.match_text_at(1, 1, "hello"), "offset by one misses");
        assert!(
            !s.match_text_at(0, 1, "hello world!"),
            "longer than row content"
        );
    }

    #[test]
    fn precomposed_and_combining_accents() {
        // é precomposed (2 UTF-8 bytes, 1 column).
        let s = screen_with_row(10, &["é", "x"]);
        assert!(s.match_text_at(0, 1, "é"));
        assert!(s.match_text_at(1, 1, "x"));
        assert!(!s.match_text_at(0, 1, "e"), "different glyph misses");
        // e + combining acute (2 chars, still 1 column): the mark rides the
        // base cell, so the whole grapheme matches as one.
        let combined = "e\u{301}";
        let s2 = screen_with_row(10, &[combined, "y"]);
        assert!(s2.match_text_at(0, 1, combined));
        assert!(s2.match_text_at(1, 1, "y"));
    }

    #[test]
    fn cjk_wide_glyphs_address_by_cell_not_byte() {
        // "日本語" — 3 glyphs, 6 columns, 9 UTF-8 bytes. The old byte-index
        // arithmetic read row[x..x+len] and could panic or mismatch.
        let s = screen_with_row(12, &["日", "本", "語"]);
        assert!(s.match_text_at(0, 1, "日"));
        assert!(s.match_text_at(2, 1, "本"), "column 2, byte 3");
        assert!(s.match_text_at(4, 1, "語"), "column 4, byte 6");
        assert!(s.match_text_at(0, 1, "日本"));
        assert!(s.match_text_at(2, 1, "本語"));
        assert!(!s.match_text_at(1, 1, "本"), "cannot start mid-glyph");
        // A wide glyph's continuation column is not a match origin.
        assert!(!s.match_text_at(3, 1, "語"));
    }

    #[test]
    fn emoji_and_zwj_sequences() {
        // 🙂 is 4 UTF-8 bytes / 2 columns.
        let s = screen_with_row(10, &["🙂", "!", "!", "!"]);
        assert!(s.match_text_at(0, 1, "🙂"));
        assert!(s.match_text_at(2, 1, "!!!"), "text after a wide glyph");
        // ZWJ family emoji as a terminal actually stores it (vt100 appends
        // every zero-width char to the PREVIOUS cell): four cells, each
        // "base+ZWJ", each claiming 2 columns — 8 columns total. The match
        // is glyph-group by glyph-group, never by byte offset.
        let zwj = "\u{200D}";
        let s2 = screen_with_row(
            14,
            &[
                "👨\u{200D}",
                "👩\u{200D}",
                "👧\u{200D}",
                "👦",
                "e",
                "n",
                "d",
            ],
        );
        assert!(
            s2.match_text_at(0, 1, &format!("👨{zwj}👩{zwj}👧{zwj}👦")),
            "whole ZWJ sequence matches across its four cells"
        );
        assert!(s2.match_text_at(6, 1, "👦"), "last group at column 6");
        assert!(s2.match_text_at(8, 1, "end"), "text after the sequence");
        assert!(
            !s2.match_text_at(0, 1, "👨"),
            "group 1 alone is not the whole"
        );
        assert!(
            !s2.match_text_at(2, 1, "end"),
            "sequence occupies columns 0-7"
        );
    }

    #[test]
    fn last_column_and_out_of_range_are_honest() {
        let glyphs: Vec<&str> = "abc".split("").filter(|s| !s.is_empty()).collect();
        let s = screen_with_row(4, &glyphs);
        // Fully-filled row: the last glyph really sits at the last column.
        let full = screen_with_row(3, &glyphs);
        assert!(full.match_text_at(2, 1, "c"), "last column");
        assert!(!full.match_text_at(2, 1, "cd"), "needs past the edge");
        assert!(!full.match_text_at(3, 1, "c"), "column == cols");
        assert!(!full.match_text_at(9, 1, "c"), "far out of range");
        // In the padded row, "c" is at column 2 and column 3 is blank.
        assert!(s.match_text_at(2, 1, "c"));
        assert!(!s.match_text_at(3, 1, "c"));
        // Out-of-range row.
        assert!(!s.match_text_at(0, 9, "a"));
        // Empty needle trivially matches; screen with no cells misses all.
        assert!(s.match_text_at(0, 1, ""));
        let empty = ScreenState::new(10, 2);
        assert!(!empty.match_text_at(0, 0, "a"));
        assert!(empty.match_text_at(0, 0, ""));
    }

    #[test]
    fn frame_boundary_never_panics_on_any_column() {
        // Sweep every column of a wide-glyph row: no slice panics possible.
        let s = screen_with_row(8, &["界", "b"]);
        for x in 0..=u16::MAX {
            let _ = s.match_text_at(x, 1, "界");
        }
        // Diagnostics path: huge x/y must not index strings either.
        let _ = format!("{:?}", s.match_text_at(u16::MAX, u16::MAX, "界"));
    }

    #[test]
    fn sparse_row_blank_fill_matches_spaces() {
        // Cells only at 0 and 5; the gap is blank columns.
        let mut s = ScreenState::new(10, 2);
        s.cells.push(cell(0, 1, "a"));
        s.cells.push(cell(5, 1, "b"));
        assert!(s.match_text_at(0, 1, "a"));
        assert!(s.match_text_at(5, 1, "b"));
        assert!(s.match_text_at(1, 1, "    "), "gap reads as blanks");
        assert!(!s.match_text_at(1, 1, "b"), "b is at column 5");
    }

    #[test]
    fn cursor_and_process_defaults_hold() {
        // Sanity: the fixture builder produces the documented shape.
        let s = screen_with_row(6, &["x"]);
        assert_eq!(s.cols, 6);
        assert_eq!(
            s.cursor,
            CursorState {
                x: 0,
                y: 0,
                visible: true
            }
        );
        assert!(!s.process.running);
        assert_eq!(s.process.exit_code, None);
        assert_eq!(s.process.exit_signal, None);
    }
}
