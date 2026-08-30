//! Column-accurate row text (re-review Wave-3 item 22).
//!
//! A terminal row is a sequence of *cells*, but a glyph can be wider than one
//! cell (CJK, emoji) or take zero columns on its own (combining marks). A
//! plain `String` assembled by concatenating cell contents therefore has no
//! fixed relationship between character index and screen column — yet every
//! semantic extractor in this crate treats char offsets as x-coordinates.
//!
//! [`CellString`] is the type that restores the invariant: **character index
//! == screen column**. It is built either from per-cell contents (the
//! authoritative path, used by [`crate::screen::from_vt`]) or from already
//! column-aligned text (tests, legacy rows). It also exposes display-width
//! arithmetic so bounds computed from labels are measured in columns, not
//! UTF-8 bytes.

use unicode_width::UnicodeWidthChar;

/// A row of screen text where char index `i` is the glyph at column `i`.
///
/// Wide glyphs contribute their char at the leading column; the trailing
/// (continuation) column holds a filler space, matching how terminals render
/// the occupied cell. Zero-width marks attach to their base char's column
/// and are *dropped* from the aligned string (they render inside the base
/// cell, not in a following one), which keeps every later char on its true
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellString {
    text: String,
}

impl CellString {
    /// Wrap text that is already column-aligned (identity map). Constructor
    /// for pre-aligned strings: tests, `viewport_text` rows built by
    /// [`from_cells`], or hand-written fixture rows.
    pub fn aligned(text: impl Into<String>) -> Self {
        CellString { text: text.into() }
    }

    /// Build one row from the cell contents of that row, left to right.
    ///
    /// `cells` yields `(column, contents)` pairs in ascending column order;
    /// `cols` is the total width of the row. Each cell's contents is placed
    /// at its column: a multi-char contents (combining sequence) keeps its
    /// marks, and the following cell starts at *its own* column regardless
    /// of how many chars were consumed, so alignment survives.
    pub fn from_cells<'a, I>(cells: I, cols: u16) -> Self
    where
        I: IntoIterator<Item = (u16, &'a str)>,
    {
        let cols = cols as usize;
        // Start from a blank row; every column gets a char.
        let mut buf: Vec<char> = vec![' '; cols];
        for (x, contents) in cells {
            let x = x as usize;
            if x >= cols {
                continue;
            }
            for (off, ch) in contents.chars().enumerate() {
                let pos = x + off;
                if pos >= cols {
                    break;
                }
                buf[pos] = ch;
            }
        }
        CellString {
            text: buf.into_iter().collect(),
        }
    }

    /// The aligned text. Character index == screen column.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Chars indexed by column — the form the extractors iterate.
    pub fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }

    /// Display width in terminal columns (wide glyph counts 2, marks 0).
    pub fn display_width(&self) -> u16 {
        display_width(&self.text)
    }

    /// The glyph at column `col`, if any.
    pub fn char_at(&self, col: u16) -> Option<char> {
        self.text.chars().nth(col as usize)
    }
}

/// Display width of a string in terminal columns.
///
/// Bounds arithmetic must use this, not `str::len()` (UTF-8 bytes) or
/// `chars().count()` (a wide glyph is one char but two columns).
pub fn display_width(text: &str) -> u16 {
    text.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vt100 probe case: wide glyph `界` at col 0 (contents "界"),
    /// empty continuation cell at col 1, `A` at col 2. Aligned output puts
    /// each char on its true column.
    #[test]
    fn wide_glyph_stays_column_aligned() {
        // vt100 grid: col 0 = "界", col 1 = "" (continuation), col 2 = "A".
        let cs = CellString::from_cells(vec![(0, "界"), (1, ""), (2, "A")], 5);
        assert_eq!(cs.as_str(), "界 A  ");
        // char index == column for every char
        assert_eq!(cs.char_at(0), Some('界'));
        assert_eq!(cs.char_at(1), Some(' '));
        assert_eq!(cs.char_at(2), Some('A'));
        // width includes row padding: 2 (界) + 1 + 1 + 2 filler = 6
        assert_eq!(cs.display_width(), 6);
    }

    #[test]
    fn ascii_rows_are_identity() {
        let cs = CellString::from_cells(vec![(0, "a"), (1, "b"), (2, "c")], 4);
        assert_eq!(cs.as_str(), "abc ");
        assert_eq!(cs.display_width(), 4); // 3 content + 1 filler
    }

    #[test]
    fn gaps_in_cell_list_are_padded() {
        // Sparse rows (only non-blank cells) still align.
        let cs = CellString::from_cells(vec![(0, "["), (5, "]")], 8);
        assert_eq!(cs.as_str(), "[    ]  ");
    }

    #[test]
    fn empty_contents_is_blank_column() {
        let cs = CellString::from_cells(vec![(0, ""), (1, "x")], 3);
        assert_eq!(cs.as_str(), " x ");
    }

    #[test]
    fn contents_overflowing_row_is_truncated() {
        let cs = CellString::from_cells(vec![(0, "abcdef")], 4);
        assert_eq!(cs.as_str(), "abcd");
    }

    #[test]
    fn aligned_constructor_is_identity() {
        let cs = CellString::aligned("[ Save ]");
        assert_eq!(cs.as_str(), "[ Save ]");
        assert_eq!(cs.display_width(), 8);
    }

    #[test]
    fn display_width_counts_columns_not_bytes() {
        // 确定: 2 chars, 6 UTF-8 bytes, 4 display columns.
        assert_eq!("确定".len(), 6);
        assert_eq!(display_width("确定"), 4);
        assert_eq!(display_width("[ Save ]"), 8);
        assert_eq!(display_width("\u{AD}"), 0); // soft hyphen: zero-width
    }

    #[test]
    fn emoji_is_two_columns() {
        assert_eq!(display_width("✅"), 2);
        assert_eq!(display_width("a✅b"), 4);
    }
}
