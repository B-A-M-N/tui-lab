//! Terminal backend abstraction (spec section 2 / 36).
//!
//! `TerminalBackend` is the contract Hermes talks to. We ship Rust-native
//! `portable-pty` + `vt100` and line-CLI and tmux-attach engines. The empty
//! `tui_test_backend` Cargo feature is an INTENT marker only — it enables
//! no code today and does not mean Microsoft `tui-test` is supported (audit
//! P1-53: an empty feature must never be counted as a borrowed
//! capability).
//!
//! IMPORTANT (spec P0): the types below describe *working* behavior. Capability
//! flags must only be set when the corresponding primitive is actually
//! implemented and exercised, never for intended behavior.

use crate::screen::ScreenState;
use std::time::Duration;

pub mod line_cli;
pub mod line_types;
pub mod pipe;
pub mod portable_pty;
pub mod tmux;
pub mod trait_def;

pub use line_cli::PtyLineBackend;
pub use pipe::PipeBackend;
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
    /// True when the backend can hand back the child's RAW protocol byte
    /// stream (`recent_raw_output`). The tmux backend cannot — it sees
    /// rendered panes, not the byte stream tmux mediates — and must say so
    /// in its capability matrix rather than leave a caller to discover the
    /// unavailability by calling and getting an error (review P1 items
    /// 17/18 "raw honesty"). Portable/line backends that retain the raw
    /// ring report true.
    pub protocol_capture: bool,
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
            // Conservative: promoted true only by backends that genuinely
            // retain the raw ring. The default profile makes no claim.
            protocol_capture: false,
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

/// Wave F item 54: the shell-integration phase of the current command,
/// derived from OSC 133 escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandState {
    /// 1-based counter of command starts seen (OSC 133;C). 0 when none yet.
    pub command_seq: u64,
    /// True between 133;C and the matching 133;D.
    pub running: bool,
    /// Exit status carried by the last 133;D;exit (None when not reported).
    pub last_exit: Option<i32>,
    /// Phase label: "prompt" | "output" | "done" (mirrors OSC 133;A/B/D).
    pub phase: &'static str,
}

/// One search hit (Wave F item 53): where the query matched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    /// "viewport" or "scrollback".
    pub region: String,
    /// 0-based row. Viewport rows are screen rows; scrollback rows index the
    /// scrollback buffer (0 = oldest retained line).
    pub row: u32,
    /// Byte offset of the match within the row text.
    pub start: u32,
    /// Length of the match in bytes.
    pub len: u32,
    /// The full row text (context; renderers may trim).
    pub line: String,
}

/// Search viewport + scrollback rows for a case-insensitive substring.
/// Shared by backends so hit semantics stay identical across engines.
pub fn search_screen(screen: &ScreenState, query: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    if query.is_empty() {
        return hits;
    }
    let q = query.to_lowercase();
    let scan = |rows: &[String], region: &str, row_base: u32, hits: &mut Vec<SearchHit>| {
        for (y, row) in rows.iter().enumerate() {
            let lower = row.to_lowercase();
            let mut from = 0;
            while let Some(rel) = lower[from..].find(&q) {
                let start = from + rel;
                hits.push(SearchHit {
                    region: region.to_string(),
                    row: row_base + y as u32,
                    start: start as u32,
                    len: q.len() as u32,
                    line: row.clone(),
                });
                from = start + q.len();
            }
        }
    };
    scan(&screen.viewport_text, "viewport", 0, &mut hits);
    scan(&screen.scrollback, "scrollback", 0, &mut hits);
    hits
}

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
    /// Kitty keyboard protocol flags currently pushed by the application
    /// (Wave F item 52). `0` when the protocol is not active. When nonzero,
    /// keys that the legacy encodings cannot express (Super-modified keys,
    /// F-keys above F12) encode as CSI-u instead of being rejected.
    pub kitty_flags: u8,
}

impl Default for InputModes {
    fn default() -> Self {
        InputModes {
            application_cursor: false,
            bracketed_paste: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::Default,
            cursor_visible: true,
            kitty_flags: 0,
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
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
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
    /// Wave F item 54: shell command edge counter (OSC 133). Starts at 1
    /// when the first `133;C` (command start) is observed; the anchored
    /// CommandDone/CommandOutput waits compare against it.
    #[serde(default)]
    pub command_seq: u64,
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
    Bell {
        /// Causality anchor (review P0, Bell race): a bell with sequence
        /// strictly greater than this resolves the wait. `None` means "the
        /// next bell after the wait begins" (the old, race-prone behavior —
        /// kept so unanchored callers still work, but action completions
        /// always anchor via [`WaitCond::anchored_to`]).
        after_bell_seq: Option<u64>,
    },
    /// Re-review P0 (AnyObservableChange): any user-observable terminal
    /// activity — a screen change, a bell, a title change, or cursor motion
    /// — with an interaction sequence strictly greater than the anchor.
    /// Backends track `interaction_seq` as the sum of their observable
    /// edge counters, so this is a *superset* of a pure screen change and
    /// genuinely distinct from [`WaitCond::ScreenChange`] / anchored
    /// `ScreenStable`.
    AnyActivity {
        after_interaction_seq: Option<u64>,
    },
    Idle {
        quiet_for: Duration,
        after_output_seq: Option<u64>,
    },
    /// Wave F item 54: the shell's currently-running command (OSC 133;C seen,
    /// no OSC 133;D yet) finishes. Anchored on the command-start sequence
    /// number so "command #N finished" is expressible even after several
    /// commands have run. `Some(seq)` requires a command-finish edge with
    /// `command_seq > seq`; `None` accepts the next finish edge.
    CommandDone {
        after_command_seq: Option<u64>,
    },
    /// Wave F item 54: the named text appeared in the *output of the command
    /// that started after* the given baseline — the output is checked against
    /// the shell-scroll captured between `after_command_seq` and the next
    /// command boundary, not the whole screen history, so a token printed by
    /// an earlier command cannot satisfy this wait.
    CommandOutput {
        text: String,
        after_command_seq: Option<u64>,
    },
}

impl WaitCond {
    /// Fill in the action-baseline from a captured event state, for conditions
    /// that carry a causality anchor but were built without one.
    ///
    /// `ScreenChange` is anchored by rewriting it into an anchored
    /// `ScreenStable` (a screen change with seq > baseline followed by the
    /// quiet interval): "the reaction to my action settled" is what action
    /// callers actually mean, and it removes the entry-pump race where the
    /// action's output is consumed by the wait's own baseline capture.
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
            WaitCond::ScreenChange => {
                return WaitCond::ScreenStable {
                    quiet_for: Duration::from_millis(40),
                    after_screen_seq: Some(baseline.screen_seq),
                };
            }
            WaitCond::CommandDone {
                after_command_seq, ..
            }
            | WaitCond::CommandOutput {
                after_command_seq, ..
            } => {
                if after_command_seq.is_none() {
                    *after_command_seq = Some(baseline.command_seq);
                }
            }
            // Review P0 (Bell race): edge-triggered conditions must anchor to
            // the pre-action counters or the bell may fire between baseline
            // capture and wait entry — then the wait blocks to timeout on an
            // event that already happened. Anchoring makes "the reaction to
            // MY action" well-defined for every edge condition.
            WaitCond::Bell { after_bell_seq } => {
                if after_bell_seq.is_none() {
                    *after_bell_seq = Some(baseline.bell_seq);
                }
            }
            WaitCond::AnyActivity {
                after_interaction_seq,
            } => {
                if after_interaction_seq.is_none() {
                    *after_interaction_seq = Some(baseline.interaction_seq);
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
pub type RecordingHookSlot =
    std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn RecordingHook>>>>;

/// Create an empty recording hook slot.
pub fn new_recording_hook_slot() -> RecordingHookSlot {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

/// The reason a wait resolved (spec section 1). Hermes should know *what*
/// condition resolved and what state existed when it did.
///
/// Re-review P0: kept for backwards compatibility, but new code should
/// prefer [`CaptureReason`] which has the full reason taxonomy and
/// supports action/scenario/audit/replay paths uniformly.
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

/// The reason a capture or wait resolved. Unified taxonomy shared by waits,
/// actions, scenarios, audits, and replay (re-review P0).
///
/// Existing `WaitReason` is a strict subset; new code paths should emit a
/// `CaptureReason` directly. `From` conversions preserve compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureReason {
    /// A screen settled to stable for the required quiet interval.
    Settled,
    /// A screen-change was detected.
    ScreenChanged,
    /// The watched text appeared on screen.
    TextMatched,
    /// The watched text disappeared from screen.
    TextAbsent,
    /// The child process exited.
    ProcessExit,
    /// The terminal bell rang.
    Bell,
    /// The terminal title changed.
    Title,
    /// No output for the configured idle interval.
    Idle,
    /// The output channel closed (pipe EOF, terminal disconnected).
    OutputClosed,
    /// The wait deadline elapsed without any other reason triggering.
    Deadline,
    /// The wait was cancelled by the caller.
    Cancelled,
}

impl From<WaitReason> for CaptureReason {
    fn from(w: WaitReason) -> Self {
        match w {
            WaitReason::Text => CaptureReason::TextMatched,
            WaitReason::TextAbsent => CaptureReason::TextAbsent,
            WaitReason::ScreenChange => CaptureReason::ScreenChanged,
            WaitReason::ScreenStable => CaptureReason::Settled,
            WaitReason::ProcessExit => CaptureReason::ProcessExit,
            WaitReason::Title => CaptureReason::Title,
            WaitReason::Bell => CaptureReason::Bell,
            WaitReason::Idle => CaptureReason::Idle,
            WaitReason::Timeout => CaptureReason::Deadline,
        }
    }
}

impl From<CaptureReason> for WaitReason {
    fn from(c: CaptureReason) -> Self {
        match c {
            CaptureReason::TextMatched => WaitReason::Text,
            CaptureReason::TextAbsent => WaitReason::TextAbsent,
            CaptureReason::ScreenChanged => WaitReason::ScreenChange,
            CaptureReason::Settled => WaitReason::ScreenStable,
            CaptureReason::ProcessExit => WaitReason::ProcessExit,
            CaptureReason::Bell => WaitReason::Bell,
            CaptureReason::Title => WaitReason::Title,
            CaptureReason::Idle => WaitReason::Idle,
            CaptureReason::Deadline => WaitReason::Timeout,
            // OutputClosed/Cancelled are not representable in the older
            // WaitReason enum — map them to Idle as the closest fallback.
            CaptureReason::OutputClosed => WaitReason::Idle,
            CaptureReason::Cancelled => WaitReason::Idle,
        }
    }
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

/// The unified capture result, shared by waits, actions, scenarios, audits,
/// and replay (re-review P0). The frame field is the **authoritative**
/// matching frame — the screen state captured at the moment the reason
/// condition resolved, NOT a separate `observe()` call afterwards.
#[derive(Debug, Clone)]
pub struct CaptureOutcome {
    /// The condition that triggered capture.
    pub reason: CaptureReason,
    /// Whether the desired condition was met (`true`) or the wait timed out
    /// / was cancelled (`false`).
    pub met: bool,
    /// Sequence number of the screen at capture time.
    pub screen_seq: u64,
    /// Sequence number of the output stream at capture time.
    pub output_seq: u64,
    /// The frame that satisfied the capture (or the last seen frame on a
    /// timeout). For `met: true`, this is the authoritative matching frame.
    pub frame: ScreenState,
    /// Elapsed time in milliseconds from anchor to capture.
    pub elapsed_ms: u64,
    /// The full frame sequence for sequence captures (re-review P0:
    /// `CaptureStrategy::Frames` must return every frame it collected, not
    /// only the last). `None` for single-frame captures.
    pub frames: Option<Vec<ScreenState>>,
}

impl CaptureOutcome {
    /// Lift a `WaitOutcome` into the new `CaptureOutcome` shape.
    pub fn from_wait(o: WaitOutcome) -> Self {
        CaptureOutcome {
            reason: o.reason.into(),
            met: o.met,
            screen_seq: o.screen_seq,
            output_seq: o.output_seq,
            frame: o.state,
            elapsed_ms: o.elapsed_ms,
            frames: None,
        }
    }
}

/// The canonical frame exchanged by transactions, replay, and audit
/// evidence (re-review P0, made real in Wave B item 11). Wraps
/// [`ScreenState`] with provenance (run/session/generation), capture
/// sequence numbers, and a stable per-run `frame_id` so evidence can cite
/// `frame:1047` instead of an opaque hash. The underlying
/// [`ScreenState::structure_hash`] remains the de-duplication key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanonicalFrame {
    /// The screen at the moment the frame was captured.
    pub state: ScreenState,
    /// Screen sequence number at capture time.
    pub screen_seq: u64,
    /// Output sequence number at capture time.
    pub output_seq: u64,
    /// Per-run monotonic frame id (the citable identity: `frame:1047`).
    /// `None` until a run assigns it.
    pub frame_id: Option<u64>,
    /// The run this frame belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The session this frame was captured from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Session generation at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// Unix-millis capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<u64>,
    /// The FUSED semantic identity established at capture time (review P0.5):
    /// structure + interaction + native overlay, as reported by the observed
    /// session. Evidence persistence must serialize established truth, never
    /// recompute it — `commit_frame` uses this instead of re-inferring a bare
    /// identity from the cell grid, which would silently drop native-only
    /// state changes (pixels unchanged, focus moved) from persisted evidence.
    /// `None` when the capture path did not fuse the frame (then consumers
    /// fall back to the bare grid identity, which is the pre-P0.5 behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_identity: Option<String>,
}

impl CanonicalFrame {
    /// Wrap a `ScreenState` plus its sequence numbers into a canonical frame.
    pub fn new(state: ScreenState, screen_seq: u64, output_seq: u64) -> Self {
        CanonicalFrame {
            state,
            screen_seq,
            output_seq,
            frame_id: None,
            run_id: None,
            session_id: None,
            generation: None,
            captured_at: None,
            semantic_identity: None,
        }
    }

    /// Fill in run/session provenance (chainable).
    pub fn with_provenance(mut self, run_id: &str, session_id: &str, generation: u32) -> Self {
        self.run_id = Some(run_id.to_string());
        self.session_id = Some(session_id.to_string());
        self.generation = Some(generation);
        self.captured_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        );
        self
    }

    /// Assign the next per-run frame id (caller allocates from the run
    /// counter); returns the id for citing.
    pub fn assign_frame_id(&mut self, id: u64) -> u64 {
        self.frame_id = Some(id);
        id
    }

    /// Citable identity string (`frame:1047`) — falls back to the sequence
    /// numbers when no run id was assigned.
    pub fn cite(&self) -> String {
        match self.frame_id {
            Some(id) => format!("frame:{id}"),
            None => format!("frame:seq-{}-{}", self.screen_seq, self.output_seq),
        }
    }

    /// Convenience: the structure hash used as the canonical key.
    pub fn key(&self) -> &str {
        &self.state.structure_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CursorState, ProcessState};

    fn empty_state(hash: &str) -> ScreenState {
        ScreenState {
            cols: 80,
            rows: 24,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: vec![],
            viewport_text: vec![],
            scrollback: vec![],
            hyperlinks: Vec::new(),
            raw_hash: "r".into(),
            visual_hash: "v".into(),
            structure_hash: hash.into(),
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
    fn capture_reason_round_trips_through_wait_reason() {
        for r in [
            CaptureReason::Settled,
            CaptureReason::ScreenChanged,
            CaptureReason::TextMatched,
            CaptureReason::TextAbsent,
            CaptureReason::ProcessExit,
            CaptureReason::Bell,
            CaptureReason::Title,
            CaptureReason::Idle,
            CaptureReason::OutputClosed,
            CaptureReason::Deadline,
            CaptureReason::Cancelled,
        ] {
            let w: WaitReason = r.into();
            let back: CaptureReason = w.into();
            // OutputClosed/Cancelled map to Idle on the WaitReason side;
            // everything else is a stable round-trip.
            if matches!(r, CaptureReason::OutputClosed | CaptureReason::Cancelled) {
                assert!(matches!(back, CaptureReason::Idle));
            } else {
                assert_eq!(back, r, "round-trip for {:?}", r);
            }
        }
    }

    #[test]
    fn capture_outcome_from_wait_carries_frame() {
        let mut frame = empty_state("abc");
        frame.viewport_text = vec!["hello".into()];
        let wait = WaitOutcome {
            met: true,
            elapsed_ms: 42,
            reason: WaitReason::ScreenStable,
            screen_seq: 11,
            output_seq: 7,
            state: frame,
        };
        let cap = CaptureOutcome::from_wait(wait);
        assert!(cap.met);
        assert_eq!(cap.elapsed_ms, 42);
        assert_eq!(cap.frame.structure_hash, "abc");
        assert_eq!(cap.screen_seq, 11);
        assert_eq!(cap.output_seq, 7);
        assert!(matches!(cap.reason, CaptureReason::Settled));
    }

    #[test]
    fn canonical_frame_key_uses_structure_hash() {
        let frame = CanonicalFrame::new(empty_state("hash42"), 0, 0);
        assert_eq!(frame.key(), "hash42");
    }
}
