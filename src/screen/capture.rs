//! Screen capture for human debugging (Wave F item 57).
//!
//! Renders a [`ScreenState`] to SVG (vector, styled, inspectable in any
//! browser) or PNG (raster, stored-deflate — no external image crates).
//! These are debugging artifacts for *humans*: the agent's observation
//! source remains the cell grid. Colors map the model honestly —
//! `Color::unknown()` (default terminal color) renders as the terminal's
//! conventional dark background / light foreground, ANSI palette slots use
//! the standard xterm 256-color table, RGB passes through.

use super::cell::{Cell, Color, ScreenState};

/// Render the screen to an SVG document. Cell grid preserved exactly: each
/// cell is a fixed-width monospace glyph positioned on the grid.
pub fn to_svg(screen: &ScreenState) -> String {
    let cw = 8.0_f64; // cell width px
    let ch = 16.0_f64; // cell height px
    let pad = 4.0_f64;
    let w = screen.cols as f64 * cw + pad * 2.0;
    let h = screen.rows as f64 * ch + pad * 2.0;
    let mut out = String::with_capacity(64 * 1024);
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"'DejaVu Sans Mono','Menlo','Consolas',monospace\">\n"
    ));
    out.push_str(&format!(
        "<title>{}</title>\n",
        escape_xml(screen.title.as_deref().unwrap_or("tui-lab capture"))
    ));
    // Background: conventional terminal ground (default color → dark).
    out.push_str(&format!(
        "<rect width=\"{w}\" height=\"{h}\" fill=\"{}\"/>\n",
        css(default_bg())
    ));

    // Runs of same-style text per row (SVG text elements, not per-cell rects
    // for blanks — keeps the document small for 200x50 screens).
    for y in 0..screen.rows as usize {
        let mut x = 0;
        while x < screen.cols as usize {
            let cell = match cell_at(screen, x, y) {
                Some(c) => c,
                None => {
                    x += 1;
                    continue;
                }
            };
            if cell.is_blank() && !cell.reverse && !cell.underline {
                x += 1;
                continue;
            }
            // Extend the run while style matches.
            let mut run = String::new();
            let start_x = x;
            while x < screen.cols as usize {
                let Some(c) = cell_at(screen, x, y) else {
                    break;
                };
                if !same_style(&cell, c) {
                    break;
                }
                run.push_str(&c.text);
                x += 1;
            }
            let (fx, fy) = (pad + start_x as f64 * cw, pad + (y as f64 + 0.8) * ch);
            if cell.reverse {
                let rw = run.chars().count() as f64 * cw;
                out.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{rw}\" height=\"{ch}\" fill=\"{}\"/>\n",
                    pad + start_x as f64 * cw,
                    pad + y as f64 * ch,
                    css(fg_rgb(&cell))
                ));
            }
            out.push_str(&format!(
                "<text x=\"{fx}\" y=\"{fy}\" font-size=\"{}\" fill=\"{}\"{}{}{}>{}</text>\n",
                ch * 0.85,
                css(if cell.reverse {
                    bg_rgb(&cell)
                } else {
                    fg_rgb(&cell)
                }),
                if cell.bold {
                    " font-weight=\"bold\""
                } else {
                    ""
                },
                if cell.italic {
                    " font-style=\"italic\""
                } else {
                    ""
                },
                if cell.underline {
                    format!(" text-decoration=\"underline\"")
                } else {
                    String::new()
                },
                escape_xml(&run),
            ));
        }
    }

    // Cursor.
    if screen.cursor.visible && screen.cursor.y < screen.rows && screen.cursor.x < screen.cols {
        let (cx, cy) = (
            pad + screen.cursor.x as f64 * cw,
            pad + screen.cursor.y as f64 * ch,
        );
        out.push_str(&format!(
            "<rect x=\"{cx}\" y=\"{cy}\" width=\"{cw}\" height=\"{ch}\" fill=\"none\" stroke=\"#7aa2f7\" stroke-width=\"1.5\"/>\n"
        ));
    }
    out.push_str("</svg>\n");
    out
}

/// Render the screen to PNG bytes (RGBA, truecolor, stored-deflate zlib —
/// no external image dependency). One cell = 8x16 px.
pub fn to_png(screen: &ScreenState) -> Vec<u8> {
    const CW: usize = 8;
    const CH: usize = 16;
    let w = screen.cols as usize * CW;
    let h = screen.rows as usize * CH;

    // Simple 8x8 bitmap font for ASCII; other glyphs render as a solid
    // block (honest degradation — the SVG capture is the faithful one).
    let mut img = vec![0u8; w * h * 4];
    // Background.
    let bg = default_bg();
    for px in img.chunks_exact_mut(4) {
        px[0] = bg.0;
        px[1] = bg.1;
        px[2] = bg.2;
        px[3] = 255;
    }
    for cell in &screen.cells {
        if cell.x >= screen.cols || cell.y >= screen.rows || cell.text.is_empty() {
            continue;
        }
        let (fg, bgc) = if cell.reverse {
            (bg_rgb(&cell), fg_rgb(cell))
        } else {
            (fg_rgb(cell), bg_rgb(&cell))
        };
        // Cell background.
        draw_rect(
            &mut img,
            w,
            cell.x as usize * CW,
            cell.y as usize * CH,
            CW,
            CH,
            bgc,
        );
        if cell.is_blank() {
            continue;
        }
        // Underline strip.
        if cell.underline {
            draw_rect(
                &mut img,
                w,
                cell.x as usize * CW,
                cell.y as usize * CH + CH - 2,
                CW,
                2,
                fg,
            );
        }
        draw_glyph(
            &mut img,
            w,
            cell.x as usize * CW,
            cell.y as usize * CH,
            &cell.text,
            fg,
        );
    }
    // Cursor outline.
    if screen.cursor.visible && screen.cursor.y < screen.rows && screen.cursor.x < screen.cols {
        draw_rect_outline(
            &mut img,
            w,
            screen.cursor.x as usize * CW,
            screen.cursor.y as usize * CH,
            CW,
            CH,
            (0x7a, 0xa2, 0xf7),
        );
    }

    encode_png(&img, w as u32, h as u32)
}

// ── Color mapping ────────────────────────────────────────────────────────

/// Resolve a foreground color: RGB passes through, ANSI palette uses the
/// xterm 256 table, unknown = conventional light foreground.
fn fg_rgb(cell: &Cell) -> (u8, u8, u8) {
    palette_or(cell.fg, (0xC7, 0xC7, 0xC7))
}

/// Resolve a background color; unknown = conventional dark background.
fn bg_rgb(cell: &Cell) -> (u8, u8, u8) {
    // The model stores per-cell bg; the default remains the conventional
    // dark terminal ground.
    palette_or(cell.bg, (0x00, 0x00, 0x00))
}

/// Conventional terminal background (used for the page ground and PNG fill).
fn default_bg() -> (u8, u8, u8) {
    (0x00, 0x00, 0x00)
}

fn palette_or(c: Color, default: (u8, u8, u8)) -> (u8, u8, u8) {
    match (c.rgb, c.palette) {
        (Some((r, g, b)), _) => (r, g, b),
        (None, Some(i)) => xterm_256(i),
        (None, None) => default,
    }
}

/// xterm 256-color table (16 ANSI + 216 cube + 24 grayscale).
fn xterm_256(i: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let i = i as usize;
    if i < 16 {
        return BASE[i];
    }
    if i < 232 {
        let n = i - 16;
        let (r, g, b) = (n / 36, (n % 36) / 6, n % 6);
        let v = |x: usize| if x == 0 { 0 } else { 55 + x as u8 * 40 };
        (v(r), v(g), v(b))
    } else {
        let g = 8 + (i - 232) as u8 * 10;
        (g, g, g)
    }
}

fn css(c: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.reverse == b.reverse
}

fn cell_at(screen: &ScreenState, x: usize, y: usize) -> Option<&Cell> {
    let idx = y * screen.cols as usize + x;
    screen.cells.get(idx)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Raster primitives ────────────────────────────────────────────────────

fn draw_rect(img: &mut [u8], w: usize, x: usize, y: usize, rw: usize, rh: usize, c: (u8, u8, u8)) {
    for dy in 0..rh {
        for dx in 0..rw {
            let px = x + dx;
            let py = y + dy;
            let idx = (py * w + px) * 4;
            if idx + 3 < img.len() {
                img[idx] = c.0;
                img[idx + 1] = c.1;
                img[idx + 2] = c.2;
                img[idx + 3] = 255;
            }
        }
    }
}

fn draw_rect_outline(
    img: &mut [u8],
    w: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    c: (u8, u8, u8),
) {
    draw_rect(img, w, x, y, rw, 1, c);
    draw_rect(img, w, x, y + rh - 1, rw, 1, c);
    draw_rect(img, w, x, y, 1, rh, c);
    draw_rect(img, w, x + rw - 1, y, 1, rh, c);
}

/// 5x7 bitmap font for printable ASCII; other glyphs → solid block.
fn draw_glyph(img: &mut [u8], w: usize, ox: usize, oy: usize, text: &str, c: (u8, u8, u8)) {
    let ch = text.chars().next().unwrap_or(' ');
    if (ch as u32) < 32 || (ch as u32) > 126 {
        draw_rect(img, w, ox + 1, oy + 3, 6, 10, c);
        return;
    }
    let glyph = glyph_bits(ch);
    for (gy, row) in glyph.iter().enumerate() {
        for gx in 0..5 {
            if row & (1 << (4 - gx)) != 0 {
                // Scale 5x7 → 6x12 centered in the 8x16 cell.
                draw_rect(img, w, ox + gx + 1, oy + gy * 2 + 2, 1, 2, c);
            }
        }
    }
}

/// 5x7 glyph rows (MSB = leftmost column), classic public-domain font.
fn glyph_bits(c: char) -> [u8; 7] {
    match c {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ' ' => [0; 7],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ',' => [0, 0, 0, 0, 0x0C, 0x04, 0x08],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        ';' => [0, 0x0C, 0x0C, 0, 0x0C, 0x04, 0x08],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0, 0x04],
        '?' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0, 0x04],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1F, 0x04, 0x04, 0],
        '*' => [0, 0x15, 0x0E, 0x1F, 0x0E, 0x15, 0],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '\\' => [0x10, 0x08, 0x08, 0x04, 0x02, 0x02, 0x01],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '[' => [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E],
        ']' => [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E],
        '=' => [0, 0, 0x1F, 0, 0x1F, 0, 0],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '%' => [0x19, 0x1A, 0x02, 0x04, 0x08, 0x0B, 0x13],
        '#' => [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A],
        '@' => [0x0E, 0x11, 0x17, 0x15, 0x17, 0x10, 0x0E],
        '&' => [0x0C, 0x12, 0x14, 0x08, 0x15, 0x12, 0x0D],
        '\'' => [0x04, 0x04, 0, 0, 0, 0, 0],
        '"' => [0x0A, 0x0A, 0, 0, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0x1F],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '^' => [0x04, 0x0A, 0x11, 0, 0, 0, 0],
        '~' => [0, 0, 0x08, 0x15, 0x02, 0, 0],
        '`' => [0x08, 0x04, 0, 0, 0, 0, 0],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}

// ── PNG encoding (no external crates) ────────────────────────────────────

/// CRC-32 (PNG chunk checksum).
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, entry) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Encode raw RGBA rows into a PNG (color type 6, 8-bit, stored deflate).
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    push_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT: each scanline prefixed with filter byte 0 (None), then zlib
    // wrapper (0x78 0x01) around stored-deflate blocks.
    let stride = w as usize * 4;
    let mut raw = Vec::with_capacity(h as usize * (stride + 1));
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }
    let compressed = zlib_stored(&raw);
    push_chunk(&mut out, b"IDAT", &compressed);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// zlib wrapper (CMF=0x78, FLG=0x01) around stored (uncompressed) deflate
/// blocks of at most 65535 bytes.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut z = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    z.push(0x78);
    z.push(0x01);
    let mut chunks = raw.chunks(65535).peekable();
    if raw.is_empty() {
        z.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(c) = chunks.next() {
        let last = chunks.peek().is_none();
        z.push(if last { 0x01 } else { 0x00 });
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    // Adler-32 (not CRC-32) terminates the zlib stream.
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    for &b in raw {
        s1 = (s1 + b as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    z.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());
    z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::cell::{CursorState, ProcessState};

    fn screen(rows: usize, cols: usize, text: &[&str]) -> ScreenState {
        let mut cells = Vec::new();
        let mut viewport = Vec::new();
        for (y, line) in text.iter().enumerate() {
            viewport.push(format!("{:width$}", line, width = cols));
            for x in 0..cols {
                let ch = line.chars().nth(x).unwrap_or(' ');
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
                    reverse: x < 4,
                    strike: false,
                });
            }
        }
        for y in text.len()..rows {
            viewport.push(" ".repeat(cols));
            for x in 0..cols {
                cells.push(Cell {
                    x: x as u16,
                    y: y as u16,
                    text: " ".into(),
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
            cols: cols as u16,
            rows: rows as u16,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: Some("capture test".into()),
            cells,
            viewport_text: viewport,
            scrollback: vec![],
            hyperlinks: vec![],
            raw_hash: "r".into(),
            visual_hash: "v".into(),
            structure_hash: "s".into(),
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
    fn svg_contains_text_runs_and_dimensions() {
        let s = screen(3, 20, &["HELLO WORLD", "", "second row"]);
        let svg = to_svg(&s);
        assert!(svg.starts_with("<svg"), "svg root");
        // The fixture reverse-highlights the first 4 cells, so "HELLO WORLD"
        // legitimately splits into a reverse run and a normal run — both
        // fragments must appear.
        assert!(svg.contains("HELL"), "reverse run present: {svg}");
        assert!(svg.contains("O WORLD"), "plain run present: {svg}");
        assert!(svg.contains("</svg>"));
        assert!(svg.contains("capture test"), "title carried");
    }

    #[test]
    fn png_is_valid_structure() {
        let s = screen(4, 12, &["PNG", "rows"]);
        let png = to_png(&s);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR chunk: length 13, type IHDR.
        assert_eq!(&png[8..12], &[0, 0, 0, 13]);
        assert_eq!(&png[12..16], b"IHDR");
        // Ends with IEND + CRC.
        let tail = &png[png.len() - 12..];
        assert_eq!(&tail[..4], &[0, 0, 0, 0]);
        assert_eq!(&tail[4..8], b"IEND");
        // IDAT present.
        assert!(png.windows(4).any(|w| w == b"IDAT"));
    }

    /// The zlib wrapper decodes back to the raw filtered scanlines (round
    /// trip through a real inflate would be ideal; here we verify the stored
    /// blocks carry the exact bytes and the adler32 checksum matches).
    #[test]
    fn zlib_stored_round_trips() {
        let raw = b"hello stored-deflate world";
        let z = zlib_stored(raw);
        assert_eq!(z[0], 0x78);
        // Walk stored blocks.
        let mut i = 2;
        let mut out = Vec::new();
        loop {
            let bfinal = z[i] & 1;
            let len = u16::from_le_bytes([z[i + 1], z[i + 2]]) as usize;
            out.extend_from_slice(&z[i + 5..i + 5 + len]);
            i += 5 + len;
            if bfinal == 1 {
                break;
            }
        }
        assert_eq!(out, raw);
        // Adler32 present and correct.
        let mut s1: u32 = 1;
        let mut s2: u32 = 0;
        for &b in raw {
            s1 = (s1 + b as u32) % 65521;
            s2 = (s2 + s1) % 65521;
        }
        let stored = u32::from_be_bytes([z[i], z[i + 1], z[i + 2], z[i + 3]]);
        assert_eq!(stored, (s2 << 16) | s1);
    }

    #[test]
    fn crc32_known_vector() {
        // Standard check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// ANSI palette colors resolve through the xterm table in SVG output.
    #[test]
    fn palette_color_maps_in_svg() {
        let mut s = screen(2, 10, &["ab"]);
        if let Some(c) = s.cells.get_mut(0) {
            c.fg = Color {
                rgb: None,
                palette: Some(1),
            };
        }
        let svg = to_svg(&s);
        assert!(svg.contains("#cd0000"), "ANSI 1 → red: {svg}");
    }
}
