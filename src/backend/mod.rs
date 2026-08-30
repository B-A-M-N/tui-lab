//! Terminal backend abstraction (spec section 2 / 36).
//!
//! `TerminalBackend` is the contract Hermes talks to. We ship a Rust-native
//! `portable-pty` + `vt100` implementation (the sanctioned fallback engine) and
//! leave a `tui_test_backend` feature slot for Microsoft `tui-test` once it
//! stabilizes. No `tui-test` types leak through this trait.
//!
//! IMPORTANT (spec P0): the types below describe *working* behavior. Capability
//! flags must only be set when the corresponding primitive is actually
//! implemented and exercised, never for intended behavior.

use crate::screen::ScreenState;
use std::time::Duration;

pub mod portable_pty;
pub mod trait_def;

pub use portable_pty::PortablePtyBackend;
pub use trait_def::TerminalBackend;

/// Result of a single observation round-trip: the new screen plus how long the
/// process took to settle (if a quiescence check was requested).
#[derive(Debug, Clone)]
pub struct ObserveResult {
    pub screen: ScreenState,
    /// Whether the screen went "stable" (no cell change) within the wait budget.
    pub stable: bool,
    /// Quiet interval actually observed before declaring stability (ms).
    pub stable_ms: u64,
}

/// Backend capability flags returned on session start (spec section 41).
///
/// The `supported` boolean encodes *working behavior only*. Optional or
/// negotiated features (title, scrollback, mouse) start `false` and are
/// promoted to `true` only once the backend has evidence they function for the
/// running process. This prevents Hermes from trusting a capability that the
/// backend only intends to support.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub mouse: bool,
    pub kitty_keyboard: bool,
    pub colors: bool,
    pub cell_attributes: bool,
    pub title: bool,
    pub scrollback: bool,
    pub bracketed_paste: bool,
    pub signals: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            mouse: false, // negotiated per application; promoted on first mouse mode
            kitty_keyboard: false,
            colors: true,
            cell_attributes: true,
            title: false,           // promoted when the first OSC title is observed
            scrollback: false,      // promoted only when scrollback rows are actually captured
            bracketed_paste: false, // reported true once we can see the negotiation
            signals: cfg!(unix),    // arbitrary POSIX signals require killpg (Unix only)
        }
    }
}

impl Capabilities {
    /// Honest, dependency-light capability probe run after process start.
    ///
    /// Title/scrollback remain `false` until the running application actually
    /// emits the corresponding escape traffic; we cannot claim them up front.
    pub fn honest() -> Self {
        Capabilities::default()
    }
}

/// Generic backend error.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend spawn failed: {0}")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("process exited with {0}")]
    Exited(String),
    #[error("unsupported backend operation: {0}")]
    Unsupported(String),
    #[error("no active session")]
    NoSession,
}

pub type BackendResult<T> = Result<T, BackendError>;

/// Terminal input-mode state, derived from the parsed terminal state.
///
/// Input encoding depends on this state. For example, paste bytes are only
/// emitted when bracketed paste is negotiated, and mouse reports are only
/// emitted when the application actually requested a mouse protocol.
/// (spec section 4)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_mode: MouseMode,
    pub mouse_encoding: MouseEncoding,
    pub cursor_visible: bool,
}

impl Default for InputModes {
    fn default() -> Self {
        InputModes {
            application_cursor: false,
            bracketed_paste: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::Default,
            cursor_visible: true,
        }
    }
}

/// Negotiated mouse protocol mode (vt100 `MouseProtocolMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

impl From<vt100::MouseProtocolMode> for MouseMode {
    fn from(m: vt100::MouseProtocolMode) -> Self {
        match m {
            vt100::MouseProtocolMode::None => MouseMode::None,
            vt100::MouseProtocolMode::Press => MouseMode::Press,
            vt100::MouseProtocolMode::PressRelease => MouseMode::PressRelease,
            vt100::MouseProtocolMode::ButtonMotion => MouseMode::ButtonMotion,
            vt100::MouseProtocolMode::AnyMotion => MouseMode::AnyMotion,
        }
    }
}

/// Negotiated mouse encoding (vt100 `MouseProtocolEncoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    Default,
    Utf8,
    Sgr,
}

impl From<vt100::MouseProtocolEncoding> for MouseEncoding {
    fn from(e: vt100::MouseProtocolEncoding) -> Self {
        match e {
            vt100::MouseProtocolEncoding::Default => MouseEncoding::Default,
            vt100::MouseProtocolEncoding::Utf8 => MouseEncoding::Utf8,
            vt100::MouseProtocolEncoding::Sgr => MouseEncoding::Sgr,
        }
    }
}

/// A typed key event with modifiers (spec section 6). Replaces the fragile
/// `String` key parsing that could not even encode `ctrl+c` (`"ctrl+c"` is
/// six characters, so `s.len() == 3` never matched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    pub fn new(code: KeyCode) -> Self {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }
    pub fn with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> Self {
        KeyEvent { code, modifiers }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
}

bitflags::bitflags! {
    /// Keyboard modifier flags (spec section 6).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct KeyModifiers: u8 {
        const NONE = 0;
        const CTRL = 1;
        const ALT = 2;
        const SHIFT = 4;
        const SUPER = 8;
    }
}

impl KeyModifiers {
    pub fn ctrl(&self) -> bool {
        self.contains(KeyModifiers::CTRL)
    }
    pub fn alt(&self) -> bool {
        self.contains(KeyModifiers::ALT)
    }
    pub fn shift(&self) -> bool {
        self.contains(KeyModifiers::SHIFT)
    }
}

/// Typed mouse button (spec section 5). The MCP boundary must normalize the
/// numeric button into this enum rather than forwarding an arbitrary `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// Map a normalized `MouseButton` to the SGR button base (before
    /// release/motion bits).
    pub fn sgr_base(&self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }
}

/// A typed mouse event (spec section 5). The backend encoder translates this
/// into the negotiated protocol bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Press {
        button: MouseButton,
        x: u16,
        y: u16,
    },
    Release {
        button: MouseButton,
        x: u16,
        y: u16,
    },
    Move {
        x: u16,
        y: u16,
    },
    Drag {
        button: MouseButton,
        x: u16,
        y: u16,
    },
    Scroll {
        direction: ScrollDirection,
        x: u16,
        y: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Input event kinds (spec section 13 `tui_act`). All variants are typed;
/// the MCP layer parses human strings into these once.
#[derive(Debug, Clone)]
pub enum Input {
    Key(KeyEvent),
    Keys(Vec<KeyEvent>),
    Text(String),
    Paste(String),
    Raw(Vec<u8>),
    Mouse(MouseEvent),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Deliver the requested POSIX signal to the child process group.
    Signal(i32),
}

/// Per-event-sequence counters used for edge-triggered waits (spec section 1).
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalEventState {
    pub output_seq: u64,
    pub screen_seq: u64,
    pub bell_seq: u64,
    pub title_seq: u64,
    pub last_output_at: u64,
    pub last_screen_change_at: u64,
}

/// Wait conditions (spec section 13 `tui_wait`). All conditions are now real:
/// `ScreenChange` and `ScreenStable` are driven by the event sequencer rather
/// than constant `false`/`true` placeholders.
#[derive(Debug, Clone)]
pub enum WaitCond {
    Text(String),
    TextAbsent(String),
    ScreenChange,
    ScreenStable { quiet_for: Duration },
    ProcessExit,
    Title(String),
    Bell,
    Idle { quiet_for: Duration },
}

/// The reason a wait resolved (spec section 1). Hermes should know *what*
/// condition resolved and what state existed when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    Text,
    TextAbsent,
    ScreenChange,
    ScreenStable,
    ProcessExit,
    Title,
    Bell,
    Idle,
    Timeout,
}

/// Richer wait result replacing the bare `bool` (spec section 1).
#[derive(Debug, Clone)]
pub struct WaitOutcome {
    pub met: bool,
    pub elapsed_ms: u64,
    pub reason: WaitReason,
    pub screen_seq: u64,
    pub output_seq: u64,
    pub state: ScreenState,
}
