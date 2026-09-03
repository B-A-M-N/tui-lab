//! Terminal-cell addressing for one screen row (Wave C item 30).
//!
//! A terminal row is a sequence of *cells*, but a glyph can be wider than one
//! cell (CJK, emoji) or take zero columns on its own (combining marks). The
//! old [`CellString`] restored the invariant "char index == screen column" by
//! padding wide glyphs with a filler space at their continuation column and
//! *dropping* combining marks. That kept column arithmetic honest but lost
//! information: the row could no longer say which chars belonged to which
//! cell, marks vanished from the text, and every consumer had to treat the
//! row as a `String` again — at which point byte/char/column confusion crept
//! back in through slicing.
//!
//! The Wave C model addresses *cells* directly:
//!
//! * A row is a list of [`CellSlot`]s: `{col, text, continuation}`. A slot's
//!   `text` is the full grapheme-ish contents rendered into the cell at
//!   `col` (base char + any combining marks), and `continuation` is how many
//!   *following* columns the slot's glyph also occupies (1 for a wide glyph,
//!   0 otherwise).
//! * Extractors slice and index by **column** ([`CellString::slice_cells`],
//!   [`char_at`]), never by byte offset. [`CellString::byte_to_cell`] maps a
//!   byte offset in the rendered text back to its column for the rare legacy
//!   path that starts from a `&str`.
//! * A row is rendered to a `String` only at the output boundary
//!   ([`CellString::render`], [`CellString::aligned`]); no consumer treats
//!   the rendered string as an address space.

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// One addressable cell of a screen row: the glyph that starts at `col`,
/// plus how many following columns it spills into.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellSlot {
    /// Column this slot starts at.
    pub col: u16,
    /// Full contents rendered into this cell (base char + combining marks).
    pub text: String,
    /// Columns occupied *after* `col` by the same glyph (0 or 1; a wide
    /// glyph owns its continuation column).
    pub continuation: u16,
}

impl CellSlot {
    /// Total display columns this slot occupies (`>= 1`).
    pub fn width(&self) -> u16 {
        1 + self.continuation
    }

    /// Is this slot blank (space or empty)?
    pub fn is_blank(&self) -> bool {
        self.text.is_empty() || self.text == " "
    }
}

/// A row of screen text addressed by terminal cell.
///
/// Slots are kept **dense in column order with no gaps**: every column of the
/// row is covered by exactly one slot (blank slots have `text: " "`), so
/// `slots[i].col + slots[i].width() == slots[i+1].col` and column `c` is
/// `slots[c]` whenever all preceding glyphs are narrow. Wide glyphs make the
/// mapping non-identity — which is exactly the information the slot model
/// preserves instead of hiding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellString {
    slots: Vec<CellSlot>,
    cols: u16,
}

impl CellString {
    /// Wrap pre-aligned row text (one char per column, no wide glyphs) —
    /// tests and hand-written fixture rows. Text with wide glyphs should go
    /// through [`CellString::from_text`] instead, which measures widths.
    pub fn aligned(text: impl Into<String>) -> Self {
        Self::from_text(&text.into())
    }

    /// Build from arbitrary text, grouping it the way a terminal fills
    /// cells: each base char starts one cell, and following zero-width
    /// chars (combining marks, ZWJ joiners) ride that cell — vt100 stores
    /// a cell as "one base char + zero-width chars", so a ZWJ family emoji
    /// is ONE cell holding the whole sequence. Slot width comes from the
    /// GROUP's total display width, so `👨‍👩‍👧‍👦` claims 2 columns,
    /// not 8.
    pub fn from_text(text: &str) -> Self {
        let mut slots: Vec<CellSlot> = Vec::new();
        let mut col: u16 = 0;
        let mut group = String::new();
        let mut group_col = 0u16;
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            if w == 0 && !group.is_empty() {
                // Combining mark / ZWJ: extends the current cell.
                group.push(ch);
                continue;
            }
            // A new base char flushes the previous group.
            if !group.is_empty() {
                let continuation = (group.width() as u16).saturating_sub(1);
                slots.push(CellSlot {
                    col: group_col,
                    text: std::mem::take(&mut group),
                    continuation,
                });
                col = group_col + 1 + continuation;
            }
            if w == 0 {
                // Leading zero-width char with nothing to attach to: dropped,
                // matching the old behavior.
                continue;
            }
            group_col = col;
            group.push(ch);
        }
        if !group.is_empty() {
            let continuation = (group.width() as u16).saturating_sub(1);
            slots.push(CellSlot {
                col: group_col,
                text: group,
                continuation,
            });
            col = group_col + 1 + continuation;
        }
        let cols = col;
        CellString { slots, cols }
    }

    /// Build one row from the cell contents of a vt grid, left to right.
    ///
    /// `cells` yields `(column, contents)` pairs in ascending column order.
    /// Continuation cells of wide glyphs arrive as empty contents; they are
    /// folded into the wide slot's `continuation` count. Contents holding a
    /// combining sequence stay in one slot.
    pub fn from_cells<'a, I>(cells: I, cols: u16) -> Self
    where
        I: IntoIterator<Item = (u16, &'a str)>,
    {
        let mut slots: Vec<CellSlot> = Vec::new();
        for (x, contents) in cells {
            let x = x as usize;
            if x >= cols as usize {
                continue;
            }
            if contents.is_empty() {
                // Either a blank cell or the continuation cell of a wide
                // glyph: if the previous slot is wide and its continuation
                // column is exactly `x`, this cell is already accounted for.
                // Otherwise it is a true blank at column x.
                if slots
                    .last()
                    .is_some_and(|s| s.col + s.width() - 1 == x as u16 && s.continuation > 0)
                {
                    continue;
                }
                slots.push(CellSlot {
                    col: x as u16,
                    text: String::new(),
                    continuation: 0,
                });
                continue;
            }
            // The contents' display width decides continuation columns; the
            // base char is the first non-zero-width char. A multi-char
            // contents whose width exceeds the remaining columns is capped
            // so the slot never claims columns past the end of the row.
            let w = contents.width().max(1) as u16;
            let remaining = cols.saturating_sub(x as u16);
            let continuation = w.saturating_sub(1).min(remaining.saturating_sub(1));
            slots.push(CellSlot {
                col: x as u16,
                text: contents.to_string(),
                continuation,
            });
        }
        CellString { slots, cols }
    }

    /// Slots in column order (dense: every column is covered).
    pub fn slots(&self) -> &[CellSlot] {
        &self.slots
    }

    /// Total width of the row in columns (the grid width this row came from).
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Render to a plain aligned `String` — **output boundary only**.
    ///
    /// Column `c`'s glyph appears at char index `c` (wide glyph followed by
    /// its filler space), so the rendered string is also column-indexable as
    /// long as consumers remember it is a *render*, not the storage model.
    pub fn render(&self) -> String {
        let mut buf = String::new();
        let mut col = 0u16;
        for slot in &self.slots {
            // Pad any gap (sparse rows: unlisted cells are blank).
            while col < slot.col {
                buf.push(' ');
                col += 1;
            }
            // A cell whose contents is empty renders as a blank column.
            if slot.text.is_empty() {
                buf.push(' ');
                col += 1;
                continue;
            }
            buf.push_str(&slot.text);
            for _ in 0..slot.continuation {
                // Filler: the glyph occupies this column but contributes no
                // new char — pad so later chars stay on their true columns.
                buf.push(' ');
            }
            col += slot.width();
        }
        buf
    }

    /// Legacy bridge: the rendered row (output boundary).
    pub fn as_str(&self) -> String {
        self.render()
    }

    /// The contents of the cell at column `col` (the glyph that *starts*
    /// there), or `None` when `col` is a continuation column or out of range.
    pub fn char_at(&self, col: u16) -> Option<&str> {
        if self.slots.is_empty() {
            return None;
        }
        if let Some(slot) = self.slots.first() {
            if col < slot.col {
                return None;
            }
        }
        // Dense slots: linear scan is fine for semantic extractors (rows are
        // <= a few hundred cells); could be binary search if profiling says so.
        for (i, slot) in self.slots.iter().enumerate() {
            if col == slot.col {
                return Some(&slot.text);
            }
            if col < slot.col {
                // Gap before this slot: `col` is a blank column.
                return if self.slots.get(i.wrapping_sub(1)).is_none() {
                    None
                } else {
                    match self.slots.get(i.wrapping_sub(1)) {
                        Some(prev) if col < prev.col + prev.width() => {
                            // Continuation column of the previous wide slot.
                            None
                        }
                        _ => Some(" "),
                    }
                };
            }
        }
        // Past the last slot: within the row width it's blank, not a glyph.
        None
    }

    /// The glyph at column `col` including continuation columns: the wide
    /// glyph that owns `col`, even when `col` is its second column.
    pub fn glyph_at(&self, col: u16) -> Option<&CellSlot> {
        self.slots
            .iter()
            .find(|s| col >= s.col && col < s.col + s.width())
    }

    /// Extract columns `[x0, x0+w)` as their text, blank-padded. This is the
    /// slicing primitive for semantic extractors: it works in **columns**,
    /// never bytes, and returns the glyphs' text in column order.
    pub fn slice_cells(&self, x0: u16, w: u16) -> String {
        if w == 0 {
            return String::new();
        }
        let end = x0.saturating_add(w);
        let mut out = String::new();
        let mut col = x0;
        while col < end {
            match self.glyph_at(col) {
                Some(slot) if slot.col >= x0 => {
                    out.push_str(&slot.text);
                    col += slot.width();
                }
                // A wide glyph that starts before x0 owns this column; emit a
                // blank so the output stays column-aligned, then move on.
                Some(_) => {
                    out.push(' ');
                    col += 1;
                }
                None => {
                    out.push(' ');
                    col += 1;
                }
            }
        }
        out
    }

    /// Map a byte offset in [`CellString::render`] output back to its column.
    ///
    /// The rendered string has one char per column (wide glyphs followed by
    /// filler spaces), so this is a char-count walk: a byte offset landing
    /// inside a multi-byte char maps to that char's column; offsets past the
    /// end map to `None`.
    pub fn byte_to_cell(&self, byte_off: usize) -> Option<u16> {
        let rendered = self.render();
        for (char_index, (bi, ch)) in rendered.char_indices().enumerate() {
            let end = bi + ch.len_utf8();
            if byte_off >= bi && byte_off < end {
                return Some(char_index as u16);
            }
        }
        None
    }

    /// Display width in terminal columns (wide glyph counts 2, marks 0).
    pub fn display_width(&self) -> u16 {
        self.slots.iter().map(|s| display_width(&s.text)).sum()
    }
}

/// Display width of a string in terminal columns.
///
/// Bounds arithmetic must use this, not `str::len()` (UTF-8 bytes) or
/// `chars().count()` (a wide glyph is one char but two columns).
pub fn display_width(text: &str) -> u16 {
    text.width() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vt100 probe case: wide glyph `界` at col 0 (contents "界"),
    /// empty continuation cell at col 1, `A` at col 2. The slot model keeps
    /// the glyph in slot 0 with continuation=1; A is slot 1 at col 2.
    #[test]
    fn wide_glyph_owns_its_continuation_column() {
        let cs = CellString::from_cells(vec![(0, "界"), (1, ""), (2, "A")], 5);
        let slots = cs.slots();
        assert_eq!(slots.len(), 2, "continuation cell folds into the glyph");
        assert_eq!(slots[0].col, 0);
        assert_eq!(slots[0].text, "界");
        assert_eq!(slots[0].continuation, 1);
        assert_eq!(slots[1].col, 2);
        assert_eq!(cs.render(), "界 A");
    }

    #[test]
    fn char_at_is_column_addressed() {
        let cs = CellString::from_cells(vec![(0, "界"), (1, ""), (2, "A")], 5);
        assert_eq!(cs.char_at(0), Some("界"));
        assert_eq!(cs.char_at(1), None, "continuation column has no new glyph");
        assert_eq!(cs.char_at(2), Some("A"));
        assert_eq!(cs.glyph_at(1).map(|s| s.text.as_str()), Some("界"));
    }

    /// Item 30: slice_cells works in columns and returns glyphs, not bytes.
    #[test]
    fn slice_cells_extracts_columns() {
        let cs = CellString::from_text("a界b");
        assert_eq!(cs.slice_cells(0, 4), "a界b");
        assert_eq!(cs.slice_cells(0, 1), "a");
        assert_eq!(cs.slice_cells(1, 2), "界");
        assert_eq!(cs.slice_cells(3, 1), "b");
        // Column 2 is 界's continuation column: no glyph starts there, and
        // the wide glyph is not re-emitted from its continuation column.
        assert_eq!(cs.slice_cells(2, 1), " ");
    }

    /// slice_cells across the boundary of a wide glyph starting before x0:
    /// the glyph owns the column but is not re-emitted.
    #[test]
    fn slice_cells_skips_wide_glyph_starting_earlier() {
        let cs = CellString::from_text("a界b");
        // Column 2 is 界's continuation; slice [2,4) yields blank + "b".
        assert_eq!(cs.slice_cells(2, 2), " b");
    }

    #[test]
    fn byte_to_cell_maps_rendered_offsets() {
        let cs = CellString::from_text("a界b");
        let rendered = cs.render(); // "a界 b" (界 + filler)
        let b1 = rendered.char_indices().nth(1).map(|(b, _)| b).unwrap();
        assert_eq!(cs.byte_to_cell(b1), Some(1), "byte of 界 → column 1");
        // Byte inside the multi-byte 界 char still maps to its column.
        assert_eq!(cs.byte_to_cell(b1 + 1), Some(1));
        // Past the end: None.
        assert_eq!(cs.byte_to_cell(rendered.len() + 10), None);
    }

    /// Item 30: combining marks stay with their base glyph's cell.
    #[test]
    fn combining_marks_attach_to_base_cell() {
        // e + combining acute: one visual cell, contents "e\u{301}".
        let cs = CellString::from_cells(vec![(0, "e\u{301}"), (1, "x")], 3);
        assert_eq!(cs.char_at(0), Some("e\u{301}"));
        assert_eq!(cs.char_at(1), Some("x"));
        assert_eq!(cs.render(), "e\u{301}x");
        // Via from_text the mark also folds into the base slot.
        let cs2 = CellString::from_text("e\u{301}x");
        assert_eq!(cs2.char_at(0), Some("e\u{301}"));
        assert_eq!(cs2.char_at(1), Some("x"));
    }

    #[test]
    fn ascii_rows_are_identity() {
        let cs = CellString::from_cells(vec![(0, "a"), (1, "b"), (2, "c")], 4);
        assert_eq!(cs.render(), "abc");
        assert_eq!(cs.display_width(), 3);
    }

    #[test]
    fn gaps_in_cell_list_are_padded() {
        // Sparse rows (only non-blank cells) still align.
        let cs = CellString::from_cells(vec![(0, "["), (5, "]")], 8);
        assert_eq!(cs.render(), "[    ]");
        assert_eq!(cs.char_at(3), Some(" "));
    }

    #[test]
    fn empty_contents_is_blank_column() {
        let cs = CellString::from_cells(vec![(0, ""), (1, "x")], 3);
        assert_eq!(cs.render(), " x");
        assert_eq!(cs.char_at(0), Some(""));
    }

    #[test]
    fn contents_overflowing_row_is_capped() {
        // A contents wider than the remaining row (not produced by from_vt,
        // which splits at cell boundaries, but must not panic or overflow):
        // the slot claims at most the remaining columns.
        let cs = CellString::from_cells(vec![(0, "abcdef")], 4);
        assert_eq!(cs.cols(), 4);
        assert_eq!(cs.slots()[0].width(), 4, "slot capped at row width");
    }

    #[test]
    fn aligned_constructor_is_identity() {
        let cs = CellString::aligned("[ Save ]");
        assert_eq!(cs.render(), "[ Save ]");
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
        let cs = CellString::from_text("a✅b");
        // ✅ occupies columns 1-2; b starts at column 3.
        assert_eq!(cs.glyph_at(1).map(|s| s.text.as_str()), Some("✅"));
        assert_eq!(cs.glyph_at(2).map(|s| s.text.as_str()), Some("✅"));
        assert_eq!(cs.char_at(2), None, "continuation column: no new glyph");
        assert_eq!(cs.char_at(3), Some("b"));
    }
}
