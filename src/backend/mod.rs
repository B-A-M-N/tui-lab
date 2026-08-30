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
    /// A full click: the backend emits press AND release back-to-back in the
    /// negotiated mouse protocol (audit item 8). Never silently degrade to
    /// press-only.
    MouseClick {
        button: MouseButton,
        x: u16,
        y: u16,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Deliver the requested POSIX signal to the child process group.
    Signal(i32),
}

/// Per-event-sequence counters used for edge-triggered waits (spec section 1).
///
/// Sequence semantics (frozen contract):
///   * `output_seq`    — +1 per PTY byte chunk received from the child.
///   * `content_seq`   — +1 when the *text* contents of the screen change.
///   * `screen_seq`    — +1 when the *interaction fingerprint* changes: cell
///     text, fg/bg, bold/underline/reverse, cursor position/visibility, or
///     dimensions. This is a superset of `content_seq`: a style-only change
///     (e.g. reverse-video focus moving) bumps `screen_seq` even when the text
///     is identical. `visual_seq` is kept as an explicit alias so callers can
///     name the concept they mean.
///   * `interaction_seq` — +1 whenever any user-observable event fires
///     (screen_seq, bell, or title).
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalEventState {
    pub output_seq: u64,
    pub screen_seq: u64,
    pub content_seq: u64,
    pub visual_seq: u64,
    pub interaction_seq: u64,
    pub bell_seq: u64,
    pub title_seq: u64,
    pub last_output_at: u64,
    pub last_screen_change_at: u64,
}

/// Wait conditions (spec section 13 `tui_wait`). All conditions are now real:
/// `ScreenChange` and `ScreenStable` are driven by the event sequencer rather
/// than constant `false`/`true` placeholders.
///
/// Causality (frozen contract): `after_screen_seq` / `after_output_seq` make
/// "stability *following an action*" expressible without requiring activity to
/// begin after `wait()` is entered. `None` means a generic stability wait ("the
/// screen has been quiet for `quiet_for`"). `Some(seq)` means "a screen change
/// with sequence > seq must have occurred, and the screen must then have been
/// quiet for `quiet_for`". Callers capture
/// `backend.event_state()` (or `Session::event_state()`) *before* sending the
/// action and pass it as the baseline — see [`TerminalBackend::wait_after`].
#[derive(Debug, Clone)]
pub enum WaitCond {
    Text(String),
    TextAbsent(String),
    ScreenChange,
    ScreenStable {
        quiet_for: Duration,
        after_screen_seq: Option<u64>,
    },
    ProcessExit,
    Title(String),
    Bell,
    Idle {
        quiet_for: Duration,
        after_output_seq: Option<u64>,
    },
}

impl WaitCond {
    /// Fill in the action-baseline from a captured event state, for conditions
    /// that carry a causality anchor but were built without one.
    pub fn anchored_to(mut self, baseline: &TerminalEventState) -> Self {
        match &mut self {
            WaitCond::ScreenStable {
                after_screen_seq, ..
            } => {
                if after_screen_seq.is_none() {
                    *after_screen_seq = Some(baseline.screen_seq);
                }
            }
            WaitCond::Idle {
                after_output_seq, ..
            } => {
                if after_output_seq.is_none() {
                    *after_output_seq = Some(baseline.output_seq);
                }
            }
            _ => {}
        }
        self
    }
}

/// A recording sink that receives the *raw* PTY byte stream at the byte
/// boundary (spec item 24). Implementations must be cheap and non-blocking:
/// the PTY reader thread calls `on_output` for every chunk it reads.
///
/// Security note: `on_input` receives the exact bytes sent to the child. When
/// sensitive input must not be recorded, the *caller* must not attach a hook
/// that stores it (see `RecordingPolicy`); the backend always delivers.
pub trait RecordingHook: Send + Sync + 'static {
    fn on_output(&self, bytes: &[u8]);
    fn on_input(&self, bytes: &[u8]);
    fn on_resize(&self, cols: u16, rows: u16);
}

/// Shared slot through which a [`RecordingHook`] is attached to a backend.
/// The PTY reader thread keeps a clone of the slot and locks it per chunk, so
/// a hook can be attached/detached while a session is running.
pub type RecordingHookSlot = std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn RecordingHook>>>>;

/// Create an empty recording hook slot.
pub fn new_recording_hook_slot() -> RecordingHookSlot {
    std::sync::Arc::new(std::sync::Mutex::new(None))
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
