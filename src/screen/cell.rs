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
}
