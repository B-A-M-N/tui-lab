//! Convert a parsed `vt100::Screen` into our [`ScreenState`] and compute the
//! three hashes (raw / visual / structure) from spec section 12.
//!
//! - `raw_hash`: every cell content + every attribute bit matters.
//! - `visual_hash`: ignore attributes that do not affect visible rendering
//!   (we keep fg/bg/bold/reverse/underline since they change legibility, but
//!   drop `dim`/`italic`/`strike` only when they don't change appearance — to
//!   stay conservative we include them).
//! - `structure_hash`: normalize volatile text (numbers, timers, clocks) so an
//!   animated counter doesn't look like a new state. See [`crate::screen::normalize`].
//!
//! Hashes use blake3 with a schema/version prefix (spec section 18) so run
//! artifacts are durable and cross-version content-addressable.

use vt100::Screen as VtScreen;

use super::cell::{Cell, Color, CursorState, ProcessState, ScreenState};
use super::cell_string::display_width;

fn cell_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::unknown(),
        vt100::Color::Idx(i) => Color {
            rgb: None,
            palette: Some(i),
        },
        vt100::Color::Rgb(r, g, b) => Color {
            rgb: Some((r, g, b)),
            palette: None,
        },
    }
}

pub fn from_vt(
    screen: &VtScreen,
    process: ProcessState,
    title: Option<String>,
    hyperlinks: Vec<super::cell::Hyperlink>,
) -> ScreenState {
    from_vt_with_policy(
        screen,
        process,
        title,
        hyperlinks,
        &super::normalize::NormalizationPolicy::default(),
    )
}

/// Item 48: the contract's `volatile_patterns` are part of the structure
/// hash's identity. This variant takes the caller's [`NormalizationPolicy`]
/// — the default conservative policy when no contract is loaded.
pub fn from_vt_with_policy(
    screen: &VtScreen,
    process: ProcessState,
    title: Option<String>,
    hyperlinks: Vec<super::cell::Hyperlink>,
    policy: &super::normalize::NormalizationPolicy,
) -> ScreenState {
    let rows = screen.size().0;
    let cols = screen.size().1;
    let mut cells = Vec::with_capacity((rows * cols) as usize);
    let mut viewport_text = Vec::with_capacity(rows as usize);
    let mut raw = blake3::Hasher::new();
    let mut visual = blake3::Hasher::new();
    let mut structure = blake3::Hasher::new();

    for y in 0..rows {
        // Cell texts for this row in column order; empty continuation cells
        // (wide glyphs) and blank cells contribute "" and are padded by the
        // CellString builder (item 22 — char index == screen column).
        let row_start = cells.len();
        for x in 0..cols {
            let ch = screen.cell(y, x);
            let (text, fg, bg, bold, dim, italic, underline, reverse, strike) = match ch {
                Some(c) => (
                    c.contents().to_string(),
                    cell_color(c.fgcolor()),
                    cell_color(c.bgcolor()),
                    c.bold(),
                    c.dim(),
                    c.italic(),
                    c.underline(),
                    c.inverse(),
                    false,
                ),
                None => (
                    String::new(),
                    Color::unknown(),
                    Color::unknown(),
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                ),
            };
            let cell = Cell {
                x,
                y,
                text: text.clone(),
                fg,
                bg,
                bold,
                dim,
                italic,
                underline,
                reverse,
                strike,
            };
            cells.push(cell);

            // raw: everything
            let raw_s = format!(
                "{}:{}:{:?}{:?}{}{}{}{}{}{}",
                x, y, fg, bg, bold, dim, italic, underline, reverse, strike
            );
            raw.update(text.as_bytes());
            raw.update(raw_s.as_bytes());
            // visual: visible-render-affecting only
            let vis_s = format!(
                "{}:{}:{:?}{:?}{}{}{}",
                x, y, fg, bg, bold, underline, reverse
            );
            visual.update(text.as_bytes());
            visual.update(vis_s.as_bytes());
        }
        let line = crate::screen::cell_string::CellString::from_cells(
            cells[row_start..].iter().map(|c| (c.x, c.text.as_str())),
            cols,
        );
        // Rows render at grid width: trailing blank cells pad to `cols` so
        // consumers can index by column and diff rows of equal length.
        let mut rendered = line.render();
        let w = display_width(&rendered);
        if w < cols {
            rendered.push_str(&" ".repeat((cols - w) as usize));
        }
        viewport_text.push(rendered);
    }

    // Structure hash: normalize per-row (item 19) under the caller's policy
    // (item 48 — contract volatile patterns feed the same normalization the
    // built-in classes do).
    for (y, row) in viewport_text.iter().enumerate() {
        let normalized = crate::screen::normalize::normalize_row_with(row, policy);
        let st_s = format!("{}:{}:{}", y, normalized, false);
        structure.update(st_s.as_bytes());
    }

    let cursor = screen.cursor_position();
    let cursor_state = CursorState {
        x: cursor.1,
        y: cursor.0,
        // Honor the terminal's actual cursor-visibility state (spec section 15).
        // vt100 exposes hide_cursor(); our model stores visibility.
        visible: !screen.hide_cursor(),
    };

    ScreenState {
        cols,
        rows,
        cursor: cursor_state,
        title,
        cells,
        viewport_text,
        scrollback: Vec::new(),
        hyperlinks,
        raw_hash: format!("raw:v1:{}", hex(raw.finalize().as_bytes())),
        visual_hash: format!("visual:v1:{}", hex(visual.finalize().as_bytes())),
        structure_hash: format!("structure:v1:{}", hex(structure.finalize().as_bytes())),
        process,
    }
}

fn hex(h: &[u8]) -> String {
    let mut s = String::with_capacity(h.len() * 2);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_state() -> ProcessState {
        ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        }
    }

    /// Item 22, end to end: real vt100 grid bytes → from_vt → rows in which
    /// char index == screen column even with wide glyphs present.
    #[test]
    fn from_vt_rows_stay_column_aligned_with_wide_glyphs() {
        let mut p = vt100::Parser::new(4, 10, 0);
        // 界 (width 2) at col 0, A lands at col 2; continuation cell empty.
        p.process("界A".as_bytes());
        let state = from_vt(p.screen(), process_state(), None, Vec::new());
        let row = &state.viewport_text[0];
        assert_eq!(row.chars().next(), Some('界'));
        assert_eq!(row.chars().nth(1), Some(' '), "continuation column padded");
        assert_eq!(row.chars().nth(2), Some('A'));

        // Same row via the CellString view. The rendered row's char index
        // still equals the column (filler space after 界), but aligned()
        // re-measures widths: the filler at char 1 becomes a *slot* at col 2,
        // pushing "A" to col 3 in slot space. Column addressing must be
        // rebuilt from the vt cells, not the padded render — which is exactly
        // why byte_to_cell exists: it maps the render's offsets to columns.
        let cs = crate::screen::CellString::from_cells(
            state.cells[..10].iter().map(|c| (c.x, c.text.as_str())),
            state.cols,
        );
        assert_eq!(cs.char_at(2), Some("A"), "grid cell 2 is A");
        assert_eq!(cs.glyph_at(1).map(|s| s.text.as_str()), Some("界"));
        assert!(cs.display_width() >= 3);
    }

    /// Item 22: pure-ASCII rows are byte-identical to the old assembly
    /// (including its full-width space padding of blank cells).
    #[test]
    fn from_vt_ascii_rows_unchanged() {
        let mut p = vt100::Parser::new(4, 20, 0);
        p.process(b"[ OK ] Host: db");
        let state = from_vt(p.screen(), process_state(), None, Vec::new());
        assert!(state.viewport_text[0].starts_with("[ OK ] Host: db"));
        assert_eq!(state.viewport_text[0].len(), 20, "padded to grid width");
    }
}
