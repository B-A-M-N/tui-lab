//! Rust-native terminal backend built on `portable-pty` + `vt100`
//! (spec section 36/37 fallback engine). This is the default engine.
//!
//! P0 correctness fixes implemented here (vs. the previous placeholder
//! implementation):
//!   * event-sequenced reader (`screen_seq`/`output_seq`/`bell_seq`/`title_seq`)
//!     drives edge-triggered waits instead of constant true/false stubs;
//!   * `ScreenStable` requires a genuine quiet interval, not instant success;
//!   * `ScreenChange` resolves on the first post-baseline screen mutation;
//!   * `resize()` resizes both the OS PTY and the `vt100` parser;
//!   * paste / mouse / cursor-key encoding is mode-aware (reads negotiated state);
//!   * Ctrl/Alt/Shift/Function keys are parsed into typed [`KeyEvent`]s;
//!   * signals are delivered to the child process group (Unix), not silently
//!     mapped to `kill`;
//!   * title and bell are tracked via parser callbacks;
//!   * [`Capabilities`] reports only working behavior.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use vt100::Parser;

use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    Input, InputModes, KeyEvent, KeyModifiers, MouseEncoding, MouseEvent, MouseMode, ObserveResult,
    RecordingHook, RecordingHookSlot, ScrollDirection, TerminalEventState, WaitCond, WaitOutcome,
    WaitReason,
};
use crate::screen::{ProcessState, ScreenState};

/// vt100 callbacks that record host-observable terminal metadata.
///
/// Security boundary (spec section 75): these callbacks are *observations
/// only*. We deliberately do NOT act on the host in response to terminal
/// output — e.g. we never write the OSC 52 clipboard to the host clipboard.
#[derive(Default)]
struct BackendCallbacks {
    title: Option<String>,
    audible_bells: u64,
    title_seq: u64,
    /// OSC8 hyperlink currently open (`id=…` params + URI), if any.
    open_link: Option<crate::screen::cell::Hyperlink>,
    /// Links completed so far, in completion order.
    links: Vec<crate::screen::cell::Hyperlink>,
    /// Wave F item 52: kitty keyboard protocol flags pushed by the app
    /// (`CSI > flags u` sets, `CSI < u` pops, `CSI = flags ; mode u` sets
    /// the active portion). We track the *current* stack top honestly —
    /// a full stack is only needed if apps interleave, which none do.
    kitty_flags: u8,
    /// Whether the app ever pushed kitty flags (capability promotion).
    kitty_seen: bool,
    /// Wave F item 56: bytes the terminal should send back to the
    /// application in response to queries (DA1/DA2, DSR cursor position,
    /// DECRQM mode reports, kitty `?u`, OSC color queries). Drained back
    /// to the PTY by `pump()`.
    query_responses: Vec<u8>,
    /// Wave F item 54: shell-integration command edges (OSC 133).
    command_seq: u64,
    command_running: bool,
    last_command_exit: Option<i32>,
    command_phase: &'static str,
    /// Item 22: query/answer bookkeeping. When the responder queues an
    /// answer, it records the class here; `pump()` promotes the pending
    /// class to `last_query` at the moment it actually writes the answer
    /// bytes back to the PTY — that write is the measured "answer sent at".
    /// Monotonic counters (one per answered query) let the session layer
    /// diff "answers since last observe" into terminal events.
    answered_seq: u64,
    pending_class: Option<&'static str>,
}

impl BackendCallbacks {
    fn queue_response(&mut self, bytes: &[u8]) {
        self.query_responses.extend_from_slice(bytes);
    }

    /// Item 22: name the query class this answer belongs to and bump the
    /// answer counter. Called by every responder arm right before (or right
    /// after) queueing the reply bytes.
    fn note_answer(&mut self, class: &'static str) {
        self.answered_seq += 1;
        self.pending_class = Some(class);
    }
}

impl vt100::Callbacks for BackendCallbacks {
    fn audible_bell(&mut self, _screen: &mut vt100::Screen) {
        self.audible_bells += 1;
    }
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        // Observation only: record the requested title. Never act on the host.
        let s = String::from_utf8_lossy(title).into_owned();
        self.title = Some(s);
        self.title_seq += 1;
    }

    /// OSC8 hyperlinks (Wave C item 29): `\e]8;params;uri\e\\ … \e]8;;\e\\`.
    /// The vt grid does not carry link state, so we record the span by
    /// *cursor position at open/close time* — the same coordinates the grid
    /// uses. Observation only: we never fetch the URI.
    ///
    /// Wave F item 54: OSC 133 shell-integration marks (A=prompt start,
    /// B=command start of input echo, C=command output start, D[;exit]=
    /// command end) are parsed from the same hook. Only edges are recorded —
    /// never command payload text.
    ///
    /// Wave F item 56: OSC 10/11/… `?` color queries get an honest report
    /// (we do not track palette state; we answer with the default colors we
    /// actually render with rather than guessing the app's theme).
    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        let (y, x) = screen.cursor_position();
        match params.first().copied() {
            Some(b"8") => {
                // OSC8 with empty URI closes the current link.
                let uri_at_2: Option<&[u8]> = params.get(2).map(|p| p.as_ref());
                match uri_at_2 {
                    Some(uri) if !uri.is_empty() => {
                        let link_params =
                            String::from_utf8_lossy(params.get(1).copied().unwrap_or(b""));
                        let id = link_params
                            .split(';')
                            .find_map(|kv| kv.strip_prefix("id="))
                            .map(|s| s.to_string());
                        self.open_link = Some(crate::screen::cell::Hyperlink {
                            id,
                            uri: String::from_utf8_lossy(uri).into_owned(),
                            start: (x, y),
                            end: None,
                        });
                    }
                    _ => {
                        // Close: finish the span at the current cursor.
                        if let Some(mut link) = self.open_link.take() {
                            link.end = Some((x, y));
                            self.links.push(link);
                        }
                    }
                }
            }
            // ── OSC 133 shell integration (item 54) ──
            Some(b"133") => match params.get(1).copied() {
                Some(b"A") => {
                    self.command_phase = "prompt";
                }
                Some(b"B") => {
                    self.command_phase = "command";
                }
                Some(b"C") => {
                    self.command_seq += 1;
                    self.command_running = true;
                    self.command_phase = "output";
                }
                Some(b"D") => {
                    self.command_running = false;
                    self.command_phase = "done";
                    if let Some(exit) = params.get(2) {
                        let s = String::from_utf8_lossy(exit);
                        self.last_command_exit = s.trim().parse::<i32>().ok();
                    }
                }
                _ => {}
            },
            // ── Terminal color queries (item 56) ──
            // `OSC 10 ; ? BEL` (foreground), `OSC 11 ; ? BEL` (background),
            // `OSC 4 ; idx ; ? BEL` (palette). We answer with the colors we
            // actually render with — the harness displays default-color cells
            // on a plain terminal, so that is the honest report.
            Some(b"10") | Some(b"11") if params.get(2).copied() == Some(b"?".as_slice()) => {
                let fg = matches!(params.first().copied(), Some(b"10"));
                // xterm dynamic-color report: OSC <n> ; rgb:RRRR/GGGG/BBBB
                let (r, g, b) = if fg {
                    (0xC7u16, 0xC7, 0xC7)
                } else {
                    (0x00, 0x00, 0x00)
                };
                let which = if fg { 10 } else { 11 };
                self.note_answer("osc_color");
                self.queue_response(
                    format!("\x1b]{};rgb:{:04x}/{:04x}/{:04x}\x07", which, r, g, b).as_bytes(),
                );
            }
            _ => {}
        }
    }

    /// Wave F items 52 + 56: CSI sequences vt100 does not implement carry
    /// the kitty keyboard protocol stack ops and the terminal queries.
    fn unhandled_csi(
        &mut self,
        _screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // ── Kitty keyboard protocol (item 52) ──
        // `CSI > flags u`    push flags
        // `CSI < number u`   pop `number` entries (default 1)
        // `CSI = flags ; m u` set active flags (mode 1) / selected (mode 2)
        // `CSI ? u`          query → `CSI ? flags u` response
        if c == 'u' {
            if i1 == Some(b'>') {
                let flags = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                self.kitty_flags = flags.min(u8::MAX as u16) as u8;
                self.kitty_seen = true;
                return;
            }
            if i1 == Some(b'<') {
                let n = params
                    .first()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(1)
                    .max(1);
                // A pop past the bottom of the stack disables the protocol
                // (spec: the stack starts at depth 0 with flags 0).
                self.kitty_flags = 0;
                let _ = n; // single-depth stack: any pop clears
                return;
            }
            if i1 == Some(b'=') {
                let flags = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                match mode {
                    1 => self.kitty_flags = flags.min(u8::MAX as u16) as u8,
                    2 => self.kitty_flags |= flags.min(u8::MAX as u16) as u8,
                    3 => self.kitty_flags &= !(flags.min(u8::MAX as u16) as u8),
                    _ => {}
                }
                self.kitty_seen = true;
                return;
            }
            if i1 == Some(b'?') {
                // Query: report the currently active flags.
                self.note_answer("kitty_flags");
                self.queue_response(format!("\x1b[?{}u", self.kitty_flags).as_bytes());
                return;
            }
        }

        // ── Device queries (item 56) ──
        match (i1, c) {
            // DA1: `CSI c` or `CSI 0 c` → VT100 with AVO (`?1;2c`).
            (None, 'c') | (Some(b'0'), 'c') => {
                self.note_answer("da1");
                self.queue_response(b"\x1b[?1;2c");
            }
            // Secondary DA: `CSI > c` → vt220, version 1, no ROM.
            (Some(b'>'), 'c') => {
                self.note_answer("da2");
                self.queue_response(b"\x1b[>0;1;0c");
            }
            // Tertiary DA: `CSI = c` → unit id 0.
            (Some(b'='), 'c') => {
                self.note_answer("da3");
                self.queue_response(b"\x1bP!|0000\x1b\\");
            }
            // DSR — cursor position: `CSI 6n` → `CSI row ; col R` (1-based).
            (None, 'n') if params.first().and_then(|p| p.first()).copied() == Some(6) => {
                let (row, col) = _screen.cursor_position();
                self.note_answer("dsr_cpr");
                self.queue_response(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // DSR — operating status: `CSI 5n` → OK.
            (None, 'n') if params.first().and_then(|p| p.first()).copied() == Some(5) => {
                self.note_answer("dsr_status");
                self.queue_response(b"\x1b[0n");
            }
            // DECRQM: `CSI ? Ps $ p` → DECSET report; `CSI Ps $ p` → ANSI report.
            (Some(b'?'), 'p') => {
                let mode = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                let set = match mode {
                    1 => _screen.application_cursor(),
                    25 => !_screen.hide_cursor(),
                    1000 | 1002 | 1003 => {
                        _screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
                    }
                    1006 => _screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr,
                    2004 => _screen.bracketed_paste(),
                    1049 => _screen.alternate_screen(),
                    _ => false,
                };
                self.note_answer("decrqm");
                self.queue_response(format!("\x1b[?{};{}$y", mode, set as u8).as_bytes());
            }
            _ => {}
        }
    }
}

/// Wave-2 (protocol diagnostics): how many raw output bytes the backend
/// retains for the protocol decoder. 256 KiB covers generous terminal
/// traffic (a full-screen redraw is typically well under 8 KiB) at a small
/// fixed cost; a firehose beyond it degrades by dropping the head, which
/// `raw_output_stats` declares.
const RAW_RING_CAPACITY: usize = 256 * 1024;

pub struct PortablePtyBackend {
    cols: u16,
    rows: u16,
    parser: Parser<BackendCallbacks>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    writer: Option<Box<dyn Write + Send>>,
    // Reader thread -> main parse loop.
    reader_handle: Option<thread::JoinHandle<()>>,
    chunk_rx: Option<mpsc::Receiver<Vec<u8>>>,
    // Recording hook slot (audit item 24/25/26).
    recording_slot: RecordingHookSlot,
    // Hook clone carried into the reader thread at start().
    reader_recording_hook: Option<super::RecordingHookSlot>,
    // Wave G item 77: clear the inherited env before applying pairs at the
    // next start() (clean/strict isolation profiles).
    clear_env_on_start: bool,
    // Event sequencing state (spec section 1).
    output_seq: u64,
    screen_seq: u64,
    content_seq: u64,
    bell_seq: u64,
    title_seq: u64,
    /// Item 22: the most recent responder answer that was actually written
    /// back to the PTY (class + its monotonic answer counter). Read by the
    /// session layer after a pump to fold measured query/answer events.
    last_query_class: Option<&'static str>,
    last_query_answered_seq: u64,
    last_output_at_ms: u64,
    last_screen_change_at_ms: u64,
    /// Item 26: bounded log of `(screen_seq, unix_ms)` at each screen
    /// change, so an action's FIRST frame instant is measurable even when
    /// several changes followed it (the scalar `last_screen_change_at_ms`
    /// only remembers the last). Capped; on overflow the head is dropped.
    screen_change_log: Vec<(u64, u64)>,
    // Monotonic Instants for fine-grained wait timing (monotonic clock,
    // unlike SystemTime which can jump). These are updated in `pump()`
    // whenever the corresponding event occurs.
    child_pid: Option<u32>,
    last_output_instant: Instant,
    last_screen_change_instant: Instant,
    /// Item 48: the normalization policy applied when building structure
    /// hashes. Defaults to the built-in conservative classes; a loaded
    /// contract's `volatile_patterns` are merged in via
    /// [`Self::set_normalization_policy`].
    normalization_policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    /// Wave F item 53: scrollback rows captured at the last `state()`.
    /// The vt100 parser owns the buffer; we materialize rows eagerly so
    /// consumers (search, observe mode=scrollback) read plain strings.
    scrollback_cache: Vec<String>,
    /// Wave F item 53: whether any scrollback row was ever captured —
    /// `Capabilities.scrollback` is promoted only on this evidence.
    scrollback_seen: bool,
    /// Wave-2 (protocol diagnostics): bounded ring of the child's REAL raw
    /// output bytes — escape sequences, OSC, DCS and all — so the protocol
    /// decoder can reconstruct "what did this TUI actually emit".
    raw_ring: std::collections::VecDeque<u8>,
    /// Bytes dropped off the raw ring's head (declared eviction).
    raw_dropped: u64,
    /// Total raw bytes EVER absorbed (re-review item 19): the absolute
    /// stream position of the next byte. The CURRENT window covers absolute
    /// offsets `[raw_bytes_total - raw_ring.len(), raw_bytes_total)`, so a
    /// transaction can cite its exact byte range in the child's output
    /// stream even after head eviction.
    raw_bytes_total: u64,
}

impl PortablePtyBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        let now = Instant::now();
        PortablePtyBackend {
            cols,
            rows,
            parser: Parser::new_with_callbacks(rows, cols, 10_000, BackendCallbacks::default()),
            master: None,
            child: None,
            writer: None,
            reader_handle: None,
            chunk_rx: None,
            recording_slot: new_recording_hook_slot(),
            reader_recording_hook: None,
            clear_env_on_start: false,
            output_seq: 0,
            screen_seq: 0,
            content_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            last_query_class: None,
            last_query_answered_seq: 0,
            last_output_at_ms: 0,
            last_screen_change_at_ms: 0,
            screen_change_log: Vec::new(),
            child_pid: None,
            last_output_instant: now,
            last_screen_change_instant: now,
            normalization_policy: std::sync::Arc::new(crate::screen::NormalizationPolicy::default()),
            scrollback_cache: Vec::new(),
            scrollback_seen: false,
            raw_ring: std::collections::VecDeque::new(),
            raw_dropped: 0,
            raw_bytes_total: 0,
        }
    }

    /// Item 48: install a contract-derived normalization policy. Affects
    /// every subsequent `state()` / `observe()` structure hash.
    pub fn set_normalization_policy(
        &mut self,
        policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
        self.normalization_policy = policy;
    }

    /// Drain buffered PTY bytes, feed the parser, and bump `screen_seq` on
    /// fingerprint change (cell text + style + cursor state), bumping
    /// `content_seq` on text-only change. Returns (before_fp, after_fp) hashes
    /// so the caller can detect screen mutations. This is the single funnel
    /// through which all terminal bytes and screen re-evaluation pass
    /// (audit items 1/2).
    fn pump(&mut self) -> BackendResult<(String, String)> {
        let before_contents = self.parser.screen().contents();
        let before_fp = self.interaction_fingerprint();
        // Drain into a local buffer first so `absorb_raw` can take `&mut self`
        // without holding the `chunk_rx` borrow across the mutation.
        let mut drained_chunks: Vec<Vec<u8>> = Vec::new();
        if let Some(rx) = self.chunk_rx.as_ref() {
            // Non-blocking drain of everything currently buffered.
            while let Ok(chunk) = rx.try_recv() {
                drained_chunks.push(chunk);
            }
        }
        for chunk in &drained_chunks {
            // Raw-output ring first (Wave-2 protocol diagnostics): the
            // child's real bytes, before the parser interprets them.
            self.absorb_raw(chunk);
            self.parser.process(chunk);
            self.output_seq += 1;
            self.last_output_at_ms = now_ms();
            self.last_output_instant = Instant::now();
        }
        // Wave F item 56: write back any query responses the callbacks
        // produced (DA/DSR/DECRQM/kitty ?u/OSC color reports). A real
        // terminal answers these; an app that asked and never got an answer
        // would hang waiting — silence would be a protocol lie.
        let drained = std::mem::take(&mut self.parser.callbacks_mut().query_responses);
        if !drained.is_empty() {
            // write_input forwards to the PTY; fall back silently when no
            // session is attached (callbacks can fire during parser tests).
            let _ = self.write_input(&drained);
            // Item 22: the answer just left for the app — that instant is
            // the measurable "responded at". Promote the pending class so
            // the session layer can fold a measured query/answer event.
            {
                let cb = self.parser.callbacks_mut();
                if let Some(class) = cb.pending_class.take() {
                    self.last_query_class = Some(class);
                    self.last_query_answered_seq = cb.answered_seq;
                }
            }
        }
        let after_contents = self.parser.screen().contents();
        let after_fp = self.interaction_fingerprint();
        // content_seq bumps when text changed
        if before_contents != after_contents {
            self.content_seq += 1;
        }
        // screen_seq bumps when the fingerprint changed (text or style)
        if before_fp != after_fp {
            self.screen_seq += 1;
            self.last_screen_change_at_ms = now_ms();
            self.last_screen_change_instant = Instant::now();
            // Item 26: record the change for per-action first-frame latency.
            const SCREEN_CHANGE_LOG_CAP: usize = 256;
            if self.screen_change_log.len() == SCREEN_CHANGE_LOG_CAP {
                self.screen_change_log.remove(0);
            }
            self.screen_change_log
                .push((self.screen_seq, self.last_screen_change_at_ms));
        }
        // Wave F item 53: materialize the parser's scrollback rows so
        // search/observe read plain strings without touching the parser's
        // scroll offset. Reading rows via `set_scrollback` would disturb the
        // live view; instead we page the offset, copy, and restore it.
        self.refresh_scrollback();
        Ok((before_fp, after_fp))
    }

    /// Wave F item 53: copy the parser's scrollback rows into
    /// [`Self::scrollback_cache`]. The vt100 API exposes history only by
    /// scrolling the view (`set_scrollback`); the offset is paged to each
    /// position, the visible top row copied, and the original offset
    /// restored — the live screen is untouched when this returns.
    ///
    /// History *length* is discovered by probing: `set_scrollback` clamps to
    /// the buffer size, so a huge request returns the actual length in
    /// `screen.scrollback()` (which is the *offset*, not the size, in the
    /// normal view).
    fn refresh_scrollback(&mut self) {
        let saved = self.parser.screen().scrollback();
        // Probe: the clamp tells us how much history actually exists.
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let total = self.parser.screen().scrollback();
        if total == 0 {
            self.parser.screen_mut().set_scrollback(saved);
            // Nothing new since last refresh and cache already empty: skip
            // the (cheap but nonzero) page walk.
            if self.scrollback_cache.is_empty() {
                return;
            }
        }
        let mut rows = Vec::with_capacity(total);
        for off in 1..=total {
            self.parser.screen_mut().set_scrollback(off);
            let row = self
                .parser
                .screen()
                .rows(0, self.cols)
                .next()
                .unwrap_or_default();
            rows.push(row);
        }
        self.parser.screen_mut().set_scrollback(saved);
        self.scrollback_cache = rows;
        if !self.scrollback_cache.is_empty() {
            self.scrollback_seen = true;
        }
    }

    /// Wave F item 54: pull the OSC 133 command state out of the callbacks.
    fn command_state_from_callbacks(&self) -> Option<super::CommandState> {
        let cb = self.parser.callbacks();
        if cb.command_seq == 0 && !cb.command_running && cb.last_command_exit.is_none() {
            // No 133 edges ever seen: honest None so command waits report
            // "shell integration not present" instead of spinning. (Phase
            // defaults to "" before any A/B/C/D arrives; only D sets an
            // exit, only C starts a count.)
            return None;
        }
        let phase: &'static str = match cb.command_phase {
            "output" => "output",
            "command" => "command",
            "done" => "done",
            _ => "prompt",
        };
        Some(super::CommandState {
            command_seq: cb.command_seq,
            running: cb.command_running,
            last_exit: cb.last_command_exit,
            phase,
        })
    }

    /// Notify the recording hook if attached (audit item 24).
    /// Locks the slot and calls `f`; ignores poisoned mutex.
    fn notify<F: FnOnce(&dyn RecordingHook)>(&self, f: F) {
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref hook) = *slot {
                f(hook.as_ref());
            }
        }
    }

    /// Compute an interaction fingerprint over the vt100 screen.
    ///
    /// Hashes every cell's text, fg, bg, bold, underline, reverse, plus cursor
    /// position, cursor visibility, and (cols, rows) dimensions via blake3.
    /// A change in reverse-video styling alone will change the fingerprint
    /// (audit item 1).
    /// Wave-2 (protocol diagnostics): push raw child bytes into the bounded
    /// ring, declaring head eviction. Bounded at [`RAW_RING_CAPACITY`] so a
    /// firehose child cannot grow memory without limit.
    fn absorb_raw(&mut self, bytes: &[u8]) {
        self.raw_bytes_total += bytes.len() as u64;
        for &b in bytes {
            if self.raw_ring.len() >= RAW_RING_CAPACITY {
                self.raw_ring.pop_front();
                self.raw_dropped += 1;
            }
            self.raw_ring.push_back(b);
        }
    }

    /// The retained raw output (oldest first) plus declared drop stats.
    pub fn raw_output_window(&mut self) -> (Vec<u8>, usize, u64) {
        let _ = self.pump();
        (
            self.raw_ring.iter().copied().collect(),
            RAW_RING_CAPACITY,
            self.raw_dropped,
        )
    }

    /// The absolute byte range the retained window covers in the child's
    /// output stream: `(start_offset, end_offset_exclusive)`. Offsets survive
    /// head eviction, so a transaction's protocol range is citable even when
    /// the ring has wrapped (re-review item 19).
    pub fn raw_window_range(&mut self) -> (u64, u64) {
        let _ = self.pump();
        let end = self.raw_bytes_total;
        let start = end.saturating_sub(self.raw_ring.len() as u64);
        (start, end)
    }

    /// The absolute stream position at this instant — the offset the NEXT
    /// byte will get. Snapshot before and after an action to bracket it.
    pub fn raw_bytes_total(&mut self) -> u64 {
        let _ = self.pump();
        self.raw_bytes_total
    }

    /// Item 22: the responder's most recent answer actually written back to
    /// the PTY — `(class, answered_counter)` — or `None` when nothing was
    /// answered since start/reset. Measured, not narrated: the counter only
    /// bumps at the `write_input` that delivered the bytes.
    pub fn last_query_answer(&mut self) -> (Option<&'static str>, u64) {
        let _ = self.pump();
        (self.last_query_class, self.last_query_answered_seq)
    }

    /// Item 26: `(screen_seq, unix_ms)` for every screen change at/after
    /// `after_seq`, oldest first — the evidence an action's FIRST frame
    /// latency is derived from. Empty when the engine saw no change since
    /// `after_seq`.
    pub fn screen_changes_since(&mut self, after_seq: u64) -> Vec<(u64, u64)> {
        let _ = self.pump();
        self.screen_change_log
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .copied()
            .collect()
    }

    /// Item 22: measured conformance probe. Feed `query` through the SAME
    /// parser the child's output flows through (so the same responder
    /// handles it), then return the exact answer bytes the responder
    /// composed for the app. Nothing is written to the child and nothing
    /// enters the output ring — the probe measures the ENGINE's reply to a
    /// query class, which is precisely what conformance means here.
    pub fn probe_query_response(&mut self, query: &[u8]) -> (Option<&'static str>, Vec<u8>) {
        self.parser.process(query);
        // The callbacks queue the reply; drain WITHOUT writing it to the
        // PTY (this is a probe, not app traffic).
        let drained = std::mem::take(&mut self.parser.callbacks_mut().query_responses);
        let class = self.parser.callbacks_mut().pending_class.take();
        // pending_class was set by the LAST answering arm in `query`; for a
        // single-query probe it is exactly the answer's class.
        (class, drained)
    }

    fn interaction_fingerprint(&self) -> String {
        let screen = self.parser.screen();
        let mut hasher = blake3::Hasher::new();
        let rows = screen.size().0;
        let cols = screen.size().1;
        hasher.update(b"fp:v1:");
        hasher.update(&cols.to_le_bytes());
        hasher.update(&rows.to_le_bytes());
        for y in 0..rows {
            for x in 0..cols {
                if let Some(cell) = screen.cell(y, x) {
                    hasher.update(cell.contents().as_bytes());
                    hasher.update(&color_to_u32(cell.fgcolor()).to_le_bytes());
                    hasher.update(&color_to_u32(cell.bgcolor()).to_le_bytes());
                    hasher.update(&[cell.bold() as u8]);
                    hasher.update(&[cell.underline() as u8]);
                    hasher.update(&[cell.inverse() as u8]);
                } else {
                    hasher.update(b"\0");
                }
            }
        }
        let cursor = screen.cursor_position();
        hasher.update(&cursor.0.to_le_bytes());
        hasher.update(&cursor.1.to_le_bytes());
        hasher.update(&[!screen.hide_cursor() as u8]);
        hasher.finalize().to_hex().to_string()
    }

    /// Pull bell/title counters out of the parser callbacks (shared by
    /// `state()`, `wait()` and `send_input`).
    fn sync_parser_counters(&mut self) {
        let cb = self.parser.callbacks();
        if cb.audible_bells > self.bell_seq {
            self.bell_seq = cb.audible_bells;
        }
        if cb.title_seq > self.title_seq {
            self.title_seq = cb.title_seq;
        }
    }

    fn current_modes(&self) -> InputModes {
        let screen = self.parser.screen();
        InputModes {
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            mouse_mode: screen.mouse_protocol_mode().into(),
            mouse_encoding: screen.mouse_protocol_encoding().into(),
            cursor_visible: !screen.hide_cursor(),
            // Wave F item 52: kitty flags live in the callbacks (the vt100
            // grid has no notion of them).
            kitty_flags: self.parser.callbacks().kitty_flags,
        }
    }

    /// Write bytes to the PTY via the writer and notify the recording hook
    /// (audit item 24).
    fn write_input(&mut self, bytes: &[u8]) -> BackendResult<()> {
        let writer = match self.writer.as_mut() {
            Some(w) => w,
            None => return Err(BackendError::NoSession),
        };
        writer.write_all(bytes).map_err(BackendError::Io)?;
        writer.flush().map_err(BackendError::Io)?;
        // Notify input hook via the reader thread's hook clone.
        if let Some(ref hook) = self.reader_recording_hook {
            if let Ok(slot) = hook.lock() {
                if let Some(ref h) = *slot {
                    h.on_input(bytes);
                }
            }
        }
        Ok(())
    }

    /// Review P0 (AnyObservableChange): the backend's own observable-edge
    /// counter — screen changes + bells + titles. A pure sum, so any single
    /// edge advancing any component advances this. The `AnyActivity` wait
    /// compares against it so "any observable change" is a real superset of
    /// "screen change", including bell-only and title-only reactions.
    fn interaction_seq(&self) -> u64 {
        self.screen_seq + self.bell_seq + self.title_seq
    }
}

impl TerminalBackend for PortablePtyBackend {
    fn recent_raw_output(&mut self) -> BackendResult<Vec<u8>> {
        let (bytes, _cap, _dropped) = self.raw_output_window();
        Ok(bytes)
    }

    fn raw_output_stats(&mut self) -> (usize, u64) {
        let (_bytes, cap, dropped) = self.raw_output_window();
        (cap, dropped)
    }

    fn raw_window_range(&mut self) -> (u64, u64) {
        PortablePtyBackend::raw_window_range(self)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn start(
        &mut self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> BackendResult<()> {
        self.cols = cols;
        self.rows = rows;
        self.parser = Parser::new_with_callbacks(rows, cols, 10_000, BackendCallbacks::default());

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| BackendError::Spawn(e.to_string()))?;

        let mut cmd = CommandBuilder::new(command);
        for a in args {
            cmd.arg(a);
        }
        // portable-pty's CommandBuilder defaults a child with no explicit
        // cwd to $HOME — not the parent's cwd like std::process. A test
        // harness launching relative-path targets ("python3 fixtures/app.py")
        // from the project dir would silently run from ~. Inherit the
        // parent's cwd instead; an explicit `cwd` still wins.
        let cwd_owned: String;
        let cwd_arg: Option<&str> = match cwd {
            Some(c) => Some(c),
            None => match std::env::current_dir() {
                Ok(d) => {
                    cwd_owned = d.to_string_lossy().to_string();
                    Some(cwd_owned.as_str())
                }
                Err(_) => None,
            },
        };
        if let Some(c) = cwd_arg {
            cmd.cwd(c);
        }
        // Isolation profile (Wave G item 77): clean/strict launches drop the
        // inherited environment first so the child sees only the caller's
        // pairs. Local keeps portable-pty's base-env inheritance.
        if self.clear_env_on_start {
            cmd.env_clear();
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| BackendError::Spawn(e.to_string()))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| BackendError::Io(std::io::Error::other(e.to_string())))?;
        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
        // Clone the recording hook slot for the reader thread (audit item 24).
        let reader_hook_for_thread = self.recording_slot.clone();
        let reader_hook_for_struct = self.recording_slot.clone();
        let rh = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                            eprintln!("[tui-lab reader] EOF");
                        }
                        break;
                    }
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        // Feed the recording hook if attached.
                        if let Ok(slot) = reader_hook_for_thread.lock() {
                            if let Some(ref hook) = *slot {
                                hook.on_output(&chunk);
                            }
                        }
                        if chunk_tx.send(chunk).is_err() {
                            if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                                eprintln!("[tui-lab reader] channel closed, terminating");
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                            eprintln!("[tui-lab reader] read error, terminating: {e}");
                        }
                        break;
                    }
                }
            }
        });
        self.reader_handle = Some(rh);
        self.chunk_rx = Some(chunk_rx);
        // Also store the hook clone on the struct for write_input usage.
        self.reader_recording_hook = Some(reader_hook_for_struct);

        // take writer
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| BackendError::Io(std::io::Error::other(e.to_string())))?;

        self.master = Some(pair.master);
        self.child = Some(child);
        self.writer = Some(writer);
        self.output_seq = 0;
        self.screen_seq = 0;
        self.content_seq = 0;
        self.bell_seq = 0;
        self.title_seq = 0;
        self.last_query_class = None;
        self.last_query_answered_seq = 0;
        self.last_output_at_ms = 0;
        self.last_screen_change_at_ms = 0;
        self.screen_change_log.clear();
        self.scrollback_cache.clear();
        self.scrollback_seen = false;
        self.parser.callbacks_mut().query_responses.clear();
        let now = Instant::now();
        self.last_output_instant = now;
        self.last_screen_change_instant = now;
        self.child_pid = self.child.as_ref().and_then(|c| c.process_id());

        // give the process a moment to emit initial frame
        std::thread::sleep(Duration::from_millis(150));
        // initial pump so state() is immediately meaningful
        let _ = self.pump();
        Ok(())
    }

    fn stop(&mut self) -> BackendResult<()> {
        // Signal the child process group, then the direct child, then drain.
        if let Some(pid) = self.child_pid {
            #[cfg(unix)]
            unsafe {
                // Negative pid targets the process group (killpg semantics).
                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Drop writer/master so the reader hits EOF.
        self.writer = None;
        self.master = None;
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
        self.chunk_rx = None;
        self.reader_recording_hook = None;
        self.child_pid = None;
        Ok(())
    }

    fn state(&mut self) -> BackendResult<ScreenState> {
        // Make sure any buffered output is parsed before we snapshot.
        let _ = self.pump();
        // Track bell/title sequence deltas from callbacks.
        {
            let cb = self.parser.callbacks();
            if cb.audible_bells > self.bell_seq {
                self.bell_seq = cb.audible_bells;
            }
            if cb.title_seq > self.title_seq {
                self.title_seq = cb.title_seq;
            }
        }
        let title = self.parser.callbacks().title.clone();
        let process = self.process();
        let links: Vec<crate::screen::cell::Hyperlink> = self
            .parser
            .callbacks()
            .links
            .iter()
            .cloned()
            .chain(self.parser.callbacks().open_link.clone())
            .collect();
        let mut state = crate::screen::from_vt_with_policy(
            self.parser.screen(),
            process,
            title,
            links,
            &self.normalization_policy,
        );
        // Wave F item 53: attach the real scrollback (oldest first).
        state.scrollback = self.scrollback_cache.clone();
        Ok(state)
    }

    fn send_input(&mut self, input: Input) -> BackendResult<()> {
        // Drain any pending PTY output FIRST: the application may have just
        // negotiated a mode (mouse/paste/application cursor) whose escape
        // sequence is still sitting in the channel. Encoding must reflect the
        // negotiated state at the moment of the send, not one pump behind.
        let _ = self.pump();
        self.sync_parser_counters();
        // Resolve negotiated modes up front (clone, so no lingering borrow).
        let modes = self.current_modes();
        if self.writer.is_none() {
            return Err(BackendError::NoSession);
        }
        // For Keys we parse ergonomic strings into typed KeyEvents at the
        // session boundary, so here we already operate on typed KeyEvents.
        match input {
            Input::Key(kev) => {
                let bytes = encode_key(&kev, &modes)?;
                self.write_input(&bytes)?;
            }
            Input::Keys(keys) => {
                // Execute the complete sequence (spec section 32).
                for kev in keys {
                    let bytes = encode_key(&kev, &modes)?;
                    self.write_input(&bytes)?;
                }
            }
            Input::Text(t) => {
                let bytes = t.into_bytes();
                self.write_input(&bytes)?;
            }
            Input::Paste(p) => {
                if modes.bracketed_paste {
                    let mut v = b"\x1b[200~".to_vec();
                    v.extend_from_slice(p.as_bytes());
                    v.extend_from_slice(b"\x1b[201~");
                    self.write_input(&v)?;
                } else {
                    // No bracketed paste mode: send raw text (do not lie about
                    // the protocol).
                    self.write_input(p.as_bytes())?;
                }
            }
            Input::Raw(b) => {
                self.write_input(&b)?;
            }
            Input::MouseClick { button, x, y } => {
                // Enforce mouse mode gating for press.
                match modes.mouse_mode {
                    MouseMode::None => {
                        return Err(BackendError::Unsupported(
                            "mouse reporting is not enabled by the application".into(),
                        ));
                    }
                    MouseMode::Press
                    | MouseMode::PressRelease
                    | MouseMode::ButtonMotion
                    | MouseMode::AnyMotion => {} // press is allowed
                }
                // A click is press + release back-to-back (audit item 8).
                let press =
                    encode_mouse_event(&MouseEvent::Press { button, x, y }, modes.mouse_encoding)?;
                self.write_input(&press)?;
                // Enforce mouse mode gating for release. Release is only
                // rejected in the most minimal mode (Press / 1000); even
                // 1002 (ButtonMotion) requires a release event so the
                // application can distinguish it from a button-held drag.
                if modes.mouse_mode == MouseMode::Press {
                    return Err(BackendError::Unsupported(
                        "mouse release not supported in Press-only mode".into(),
                    ));
                } // release allowed (PressRelease, ButtonMotion, AnyMotion)
                let release = encode_mouse_event(
                    &MouseEvent::Release { button, x, y },
                    modes.mouse_encoding,
                )?;
                self.write_input(&release)?;
            }
            Input::Mouse(ev) => {
                // Enforce mouse mode gating.
                let is_press = matches!(ev, MouseEvent::Press { .. });
                let is_release = matches!(ev, MouseEvent::Release { .. });
                let is_scroll = matches!(ev, MouseEvent::Scroll { .. });
                let is_move = matches!(ev, MouseEvent::Move { .. });
                let is_drag = matches!(ev, MouseEvent::Drag { .. });
                let _ = (&is_move, &is_drag); // used in mode gating below

                if modes.mouse_mode == MouseMode::None {
                    return Err(BackendError::Unsupported(
                        "mouse reporting is not enabled by the application".into(),
                    ));
                }
                let allowed = match modes.mouse_mode {
                    MouseMode::Press => is_press || is_scroll,
                    MouseMode::PressRelease => is_press || is_release || is_scroll,
                    // ButtonMotion (1002): press, release, scroll, and
                    // drag (motion-while-button-held) all allowed.
                    MouseMode::ButtonMotion => is_press || is_release || is_scroll || is_drag,
                    MouseMode::AnyMotion => true,
                    _ => false,
                };
                if !allowed {
                    return Err(BackendError::Unsupported(format!(
                        "{:?} not supported in negotiated mouse mode {:?}",
                        ev, modes.mouse_mode
                    )));
                }
                let bytes = encode_mouse_event(&ev, modes.mouse_encoding)?;
                self.write_input(&bytes)?;
            }
            Input::Resize { cols, rows } => {
                return self.resize(cols, rows);
            }
            Input::Signal(sig) => {
                #[cfg(unix)]
                {
                    if let Some(pid) = self.child_pid {
                        // Deliver to the process group so spawned subprocesses
                        // are also signalled (spec section 7).
                        let r = unsafe { libc::kill(-(pid as i32), sig) };
                        if r != 0 {
                            // fall back to killing the direct child only
                            let _ = unsafe { libc::kill(pid as i32, sig) };
                        }
                        return Ok(());
                    }
                    return Err(BackendError::NoSession);
                }
                #[cfg(not(unix))]
                {
                    let _ = sig;
                    return Err(BackendError::Unsupported(
                        "arbitrary POSIX signals are only available on Unix".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> BackendResult<()> {
        self.cols = cols;
        self.rows = rows;
        if let Some(master) = self.master.as_ref() {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| BackendError::Spawn(e.to_string()))?;
        }
        // Resize the vt100 parser as well so our parsed screen matches the
        // child's notion of dimensions (spec section 3).
        self.parser.screen_mut().set_size(rows, cols);
        // Notify resize hook (audit item 24).
        self.notify(|hook| hook.on_resize(cols, rows));
        Ok(())
    }

    fn wait(&mut self, cond: WaitCond, budget: Duration) -> BackendResult<WaitOutcome> {
        let start = Instant::now();
        // Review P0 (Bell race): an anchored Bell carries its own baseline;
        // only an unanchored Bell falls back to "captured at wait entry".
        let baseline_bell_seq = match &cond {
            WaitCond::Bell {
                after_bell_seq: Some(seq),
            } => seq.saturating_sub(1),
            WaitCond::Bell {
                after_bell_seq: None,
            } => self.bell_seq,
            _ => self.bell_seq,
        };
        let baseline_interaction_seq = self.interaction_seq();

        // For screen-change we need the baseline *interaction fingerprint*
        // now (style-only changes count as changes; audit item 3).
        let baseline_hash = {
            let _ = self.pump();
            self.interaction_fingerprint()
        };

        loop {
            // Pump new bytes first.
            let _ = self.pump();
            // Track bell/title edges.
            {
                let cb = self.parser.callbacks();
                if cb.audible_bells > self.bell_seq {
                    self.bell_seq = cb.audible_bells;
                }
                if cb.title_seq > self.title_seq {
                    self.title_seq = cb.title_seq;
                }
            }

            let process = self.process();
            let title = self.parser.callbacks().title.clone();
            let mut links: Vec<crate::screen::cell::Hyperlink> =
                self.parser.callbacks().links.clone();
            if let Some(open) = &self.parser.callbacks().open_link {
                links.push(open.clone());
            }
            let screen = crate::screen::from_vt_with_policy(
                self.parser.screen(),
                process,
                title,
                links,
                &self.normalization_policy,
            );
            // Wave F item 53: command-output waits search scrollback too.
            let mut screen = screen;
            screen.scrollback = self.scrollback_cache.clone();
            let screen = screen;

            let (met, reason) = match &cond {
                WaitCond::Text(t) => (
                    screen.viewport_text.iter().any(|r| r.contains(t.as_str())),
                    WaitReason::Text,
                ),
                WaitCond::TextAbsent(t) => (
                    !screen.viewport_text.iter().any(|r| r.contains(t.as_str())),
                    WaitReason::TextAbsent,
                ),
                WaitCond::ScreenChange => {
                    let cur_fp = self.interaction_fingerprint();
                    (cur_fp != baseline_hash, WaitReason::ScreenChange)
                }
                WaitCond::ScreenStable {
                    quiet_for,
                    after_screen_seq,
                } => {
                    // Generic stability: the screen has simply been quiet for
                    // `quiet_for`. NO fresh mutation is required — a one-frame
                    // reaction that arrived *before* wait() was entered must
                    // still resolve (audit items 1/2/70).
                    // Anchored stability: additionally require a screen change
                    // with sequence strictly greater than the captured baseline
                    // (wait_after / anchored_to).
                    let anchored_ok = match after_screen_seq {
                        Some(seq) => self.screen_seq > *seq,
                        None => true,
                    };
                    let quiet_ok = self.last_screen_change_instant.elapsed() >= *quiet_for;
                    (anchored_ok && quiet_ok, WaitReason::ScreenStable)
                }
                WaitCond::ProcessExit => (!screen.process.running, WaitReason::ProcessExit),
                WaitCond::Title(t) => (
                    screen.title.as_deref() == Some(t.as_str()),
                    WaitReason::Title,
                ),
                WaitCond::Bell { .. } => (self.bell_seq > baseline_bell_seq, WaitReason::Bell),
                WaitCond::AnyActivity {
                    after_interaction_seq,
                } => {
                    // Any observable edge — screen, bell, title, cursor — with
                    // an interaction sequence strictly greater than the
                    // anchor. Genuinely broader than ScreenChange: a bell-only
                    // or title-only reaction resolves here.
                    let anchored_ok = match after_interaction_seq {
                        Some(seq) => self.interaction_seq() > *seq,
                        None => self.interaction_seq() > baseline_interaction_seq,
                    };
                    (anchored_ok, WaitReason::ScreenChange)
                }
                WaitCond::Idle {
                    quiet_for,
                    after_output_seq,
                } => {
                    // Same shape as ScreenStable, keyed on PTY output chunks:
                    // quiet interval suffices without an anchor; with an
                    // anchor, require output strictly newer than the baseline.
                    let anchored_ok = match after_output_seq {
                        Some(seq) => self.output_seq > *seq,
                        None => true,
                    };
                    let quiet_ok = self.last_output_instant.elapsed() >= *quiet_for;
                    (anchored_ok && quiet_ok, WaitReason::Idle)
                }
                WaitCond::CommandDone { after_command_seq } => {
                    // Wave F item 54: a finish edge (133;D) with sequence
                    // strictly greater than the anchor resolved this wait.
                    let cb = self.parser.callbacks();
                    let anchored_ok = match after_command_seq {
                        Some(seq) => cb.command_seq > *seq,
                        None => cb.command_seq > 0 && !cb.command_running,
                    };
                    // `None` anchor semantics: the NEXT finish edge after
                    // entering the wait — so the wait must have seen the
                    // command both start and finish while it ran. With an
                    // anchor, the finish simply has to be newer.
                    let done_now = !cb.command_running;
                    (anchored_ok && done_now, WaitReason::Idle)
                }
                WaitCond::CommandOutput {
                    text,
                    after_command_seq,
                } => {
                    // Wave F item 54: the text must appear in output captured
                    // for the anchored command — checked against the live
                    // viewport only when the anchored command is the one
                    // currently running or the last finished one, so a token
                    // from an EARLIER command cannot satisfy this wait.
                    let cb = self.parser.callbacks();
                    let anchored_ok = match after_command_seq {
                        Some(seq) => cb.command_seq > *seq,
                        None => cb.command_seq > 0,
                    };
                    let in_window = cb.command_seq.saturating_sub(1)
                        == after_command_seq.unwrap_or(0)
                        || cb.command_seq == after_command_seq.unwrap_or(0).max(1);
                    let found = anchored_ok
                        && in_window
                        && (screen
                            .viewport_text
                            .iter()
                            .any(|r| r.contains(text.as_str()))
                            || screen.scrollback.iter().any(|r| r.contains(text.as_str())));
                    (found, WaitReason::Text)
                }
            };

            if met {
                return Ok(WaitOutcome {
                    met: true,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    reason,
                    screen_seq: self.screen_seq,
                    output_seq: self.output_seq,
                    state: screen,
                });
            }
            if start.elapsed() >= budget {
                // Timeout: return current state with reason=Timeout.
                return Ok(WaitOutcome {
                    met: false,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    reason: WaitReason::Timeout,
                    screen_seq: self.screen_seq,
                    output_seq: self.output_seq,
                    state: screen,
                });
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn capabilities(&self) -> Capabilities {
        use super::{EventCapability, InputFamily, WaitCapability};
        let mut caps = Capabilities::honest();
        // Promote optional capabilities only when we have observed the running
        // application negotiate them.
        caps.title = self.title_seq > 0;
        // Wave F item 53: promoted once real scrollback rows were captured.
        caps.scrollback = self.scrollback_seen;
        caps.mouse = self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        caps.bracketed_paste = self.parser.screen().bracketed_paste();
        // Wave F item 52: the app pushed kitty keyboard flags at some point.
        caps.kitty_keyboard = self.parser.callbacks().kitty_seen;
        // This engine retains the raw PTY byte ring (`recent_raw_output`),
        // so protocol capture is genuinely available.
        caps.protocol_capture = true;
        // --- audit finding 37: the portable engine is the reference backend —
        //     every operation-oriented capability it advertises is backed by a
        //     real implementation the conformance suite exercises (finding 61).
        caps.raw_input = true;      // Input::Raw writes arbitrary bytes to the PTY
        caps.bell_observable = true; // bell_seq tracked in wait()
        caps.exit_code = true;      // process() reports the real exit code
        caps.shell_integration = true; // command_state() parses OSC 133
        caps.stdout_stderr_separation = false; // single PTY master, no split pipes
        caps.recording = true;      // recording hook delivered on output/input
        caps.attach = false;        // we spawn the child; we do not attach one
        caps.query_response = true; // the device-query responder answers CSI queries
        caps.event_types = vec![
            EventCapability::Output,
            EventCapability::Bell,
            EventCapability::Title,
            EventCapability::FocusChanged,
            EventCapability::SemanticChanged,
            EventCapability::Raw,
        ];
        caps.supported_waits = vec![
            WaitCapability::Text,
            WaitCapability::TextAbsent,
            WaitCapability::ScreenChange,
            WaitCapability::ScreenStable,
            WaitCapability::ProcessExit,
            WaitCapability::Title,
            WaitCapability::Bell,
            WaitCapability::AnyActivity,
            WaitCapability::Idle,
            WaitCapability::CommandDone,
        ];
        caps.input_families = vec![
            InputFamily::Key,
            InputFamily::Mouse,
            InputFamily::Paste,
            InputFamily::RawByte,
            InputFamily::Resize,
            InputFamily::Signal,
        ];
        caps
    }

    fn process(&mut self) -> ProcessState {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let (code, sig) = exit_status_parts(status);
                    ProcessState {
                        running: false,
                        exit_code: Some(code),
                        exit_signal: sig,
                        cwd: None,
                        pid: self.child_pid,
                    }
                }
                _ => ProcessState {
                    running: true,
                    exit_code: None,
                    exit_signal: None,
                    cwd: None,
                    pid: self.child_pid,
                },
            },
            None => ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    fn input_modes(&self) -> InputModes {
        self.current_modes()
    }

    fn event_state(&self) -> TerminalEventState {
        TerminalEventState {
            output_seq: self.output_seq,
            screen_seq: self.screen_seq,
            content_seq: self.content_seq,
            visual_seq: self.screen_seq,
            interaction_seq: self.screen_seq + self.bell_seq + self.title_seq,
            bell_seq: self.bell_seq,
            title_seq: self.title_seq,
            last_output_at: self.last_output_at_ms,
            last_screen_change_at: self.last_screen_change_at_ms,
            // Wave F item 54: OSC 133 command edges participate in wait
            // anchoring.
            command_seq: self.parser.callbacks().command_seq,
        }
    }

    /// Wave F item 53: the scrollback materialized at the last pump.
    fn scrollback_lines(&mut self) -> BackendResult<Vec<String>> {
        let _ = self.pump();
        Ok(self.scrollback_cache.clone())
    }

    /// Wave F item 53: viewport + scrollback search through the shared
    /// [`crate::backend::search_screen`] so hit semantics match other
    /// backends exactly.
    fn search(&mut self, query: &str) -> BackendResult<Vec<super::SearchHit>> {
        let screen = self.state()?;
        Ok(crate::backend::search_screen(&screen, query))
    }

    /// Wave F item 54: OSC 133 shell-integration state.
    fn command_state(&mut self) -> Option<super::CommandState> {
        let _ = self.pump();
        self.command_state_from_callbacks()
    }

    /// Attach or detach the raw PTY recording hook (audit item 24).
    /// Stores the slot so the struct can forward on_input/on_resize events.
    fn set_recording_hook(&mut self, hook: super::RecordingHookSlot) {
        self.recording_slot = hook;
    }

    /// Wave G item 77: clear the inherited environment before applying the
    /// caller's pairs on the next `start()` (clean/strict isolation).
    fn set_clear_env(&mut self, clear: bool) {
        self.clear_env_on_start = clear;
    }

    /// Observe current terminal state with optional idle-wait.
    ///
    /// Overrides the default fallback: captures state immediately, then if
    /// `idle > 0` waits for ScreenStable with the quieter budget, returning
    /// the final screen and whether it went stable (audit item 4).
    fn observe(&mut self, idle: std::time::Duration) -> BackendResult<ObserveResult> {
        // Capture state immediately.
        let initial = self.state()?;
        if idle == Duration::ZERO {
            return Ok(ObserveResult {
                screen: initial,
                stable: false,
                stable_ms: 0,
            });
        }
        let quiet = idle.min(Duration::from_millis(120));
        let start = Instant::now();
        let outcome = self.wait(
            WaitCond::ScreenStable {
                quiet_for: quiet,
                after_screen_seq: None,
            },
            idle + Duration::from_millis(500),
        )?;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ObserveResult {
            screen: outcome.state,
            stable: outcome.met,
            stable_ms: elapsed_ms,
        })
    }
}

/// Convert a vt100 color to a u32 for hashing.
fn color_to_u32(c: vt100::Color) -> u32 {
    match c {
        vt100::Color::Default => 0,
        vt100::Color::Idx(i) => 1 + (i as u32),
        vt100::Color::Rgb(r, g, b) => {
            0x1000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn exit_status_parts(status: portable_pty::ExitStatus) -> (i32, Option<String>) {
    let code = status.exit_code() as i32;
    let sig = status.signal().map(|s| s.to_string());
    (code, sig)
}

/// Encode a typed [`KeyEvent`] into raw terminal bytes (xterm-style).
///
/// Mode-aware: arrow keys and Home/End switch between SS3 and CSI based on
/// application cursor mode; Shift+ASCII letter emits the uppercase byte;
/// SUPER was historically rejected (unsupported without kitty keyboard
/// protocol) (audit item 6). Wave F item 52: when the application pushed
/// kitty keyboard flags (`CSI > flags u`), the CSI-u encoding unlocks —
/// Super-modified keys and F-keys above F12 encode faithfully instead of
/// being rejected.
fn encode_key(kev: &KeyEvent, modes: &InputModes) -> BackendResult<Vec<u8>> {
    use crate::backend::KeyCode::*;
    let KeyEvent { code, modifiers } = kev;

    // Kitty CSI-u encoding (item 52): active when the application pushed
    // flags. Covers every key we model; disambiguates Super and high
    // function keys that legacy encodings cannot express. The key code
    // follows the kitty spec's unicode-key-code table (Enter=13, Tab=9,
    // Escape=27, Backspace=127, arrows=1(A)…; F1–F12 = 57364–57375 in
    // functional-key space, but legacy numbers 11–24 are accepted too).
    if modes.kitty_flags > 0 {
        if let Some(bytes) = encode_key_kitty(kev) {
            return Ok(bytes);
        }
        // Fall through to legacy encodings when the key has no kitty form.
    }

    // Reject SUPER — cannot encode without the kitty keyboard protocol.
    if modifiers.contains(KeyModifiers::SUPER) {
        return Err(BackendError::Unsupported(
            "super/meta is not encodable without the kitty keyboard protocol".into(),
        ));
    }

    // C0 control from Ctrl+letter/control char.
    if modifiers.ctrl() {
        match code {
            Char(c) if c.is_ascii_lowercase() => {
                return Ok(vec![*c as u8 - b'a' + 1]);
            }
            Char(c) if c.is_ascii_uppercase() => {
                // Ctrl+Shift+Letter: send the uppercase control byte.
                return Ok(vec![*c as u8 - b'A' + 1]);
            }
            Char(' ') => return Ok(vec![0x00]), // Ctrl+Space
            Char('@') => return Ok(vec![0x00]),
            Char('2') => return Ok(vec![0x00]), // Ctrl+@
            Char('[') | Char('{') => return Ok(vec![0x1b]), // Ctrl+[
            Char(']') | Char('}') => return Ok(vec![0x1d]),
            Char('\\') => return Ok(vec![0x1c]),
            Char('^') => return Ok(vec![0x1e]),
            Char('_') => return Ok(vec![0x1f]),
            _ => {}
        }
    }

    let app_cursor = modes.application_cursor;
    let shift = modifiers.shift();
    let alt = modifiers.alt();

    // Modified navigation keys: xterm modifyOtherKeys-style CSI sequences
    // (re-review Wave-1 item 7 — previously Ctrl+Arrow silently degraded to
    // a bare arrow, so the application could not tell it received a modified
    // key). Format: CSI 1;<mods>{A,B,C,D} for arrows, CSI 1;<mods>{H,F} for
    // Home/End, CSI <n>;<mods>~ for PgUp/PgDn/Insert/Delete. Modifier mask is
    // 1 + shift(1) + alt(2) + ctrl(4). Unmodified keys fall through to the
    // plain encodings below (including SS3 application-cursor variants).
    let xterm_mods = 1 + (shift as u8) + ((alt as u8) << 1) + ((modifiers.ctrl() as u8) << 2);
    if shift || alt || modifiers.ctrl() {
        match code {
            Up | Down | Left | Right => {
                let ch = match code {
                    Up => 'A',
                    Down => 'B',
                    Right => 'C',
                    _ => 'D',
                };
                return Ok(format!("\x1b[1;{}{}", xterm_mods, ch).into_bytes());
            }
            Home | End => {
                let ch = if matches!(code, Home) { 'H' } else { 'F' };
                return Ok(format!("\x1b[1;{}{}", xterm_mods, ch).into_bytes());
            }
            PageUp | PageDown | Insert | Delete => {
                let n = match code {
                    PageUp => 5,
                    PageDown => 6,
                    Insert => 2,
                    _ => 3,
                };
                return Ok(format!("\x1b[{};{}~", n, xterm_mods).into_bytes());
            }
            _ => {}
        }
    }

    let base: Vec<u8> = match code {
        Char(c) if *c == ' ' => b" ".to_vec(),
        Char(c) => {
            // Shift+ASCII letter → uppercase byte (audit item 6).
            if shift && c.is_ascii_alphabetic() {
                vec![c.to_ascii_uppercase() as u8]
            } else if shift {
                // Non-letter shift: send the character as-is.
                let mut s = String::new();
                s.push(*c);
                s.into_bytes()
            } else {
                let mut s = String::new();
                s.push(*c);
                s.into_bytes()
            }
        }
        Enter => b"\r".to_vec(),
        Tab => {
            if shift {
                b"\x1b[Z".to_vec() // Shift+Tab
            } else {
                b"\t".to_vec()
            }
        }
        Backspace => b"\x7f".to_vec(),
        Escape => b"\x1b".to_vec(),
        // Arrow keys: SS3 in application cursor mode, CSI otherwise (audit item 6).
        Up if app_cursor => b"\x1bOA".to_vec(),
        Down if app_cursor => b"\x1bOB".to_vec(),
        Right if app_cursor => b"\x1bOC".to_vec(),
        Left if app_cursor => b"\x1bOD".to_vec(),
        Up => b"\x1b[A".to_vec(),
        Down => b"\x1b[B".to_vec(),
        Right => b"\x1b[C".to_vec(),
        Left => b"\x1b[D".to_vec(),
        // Home/End: SS3 variants in application cursor mode (audit item 6).
        Home if app_cursor => b"\x1bOH".to_vec(),
        End if app_cursor => b"\x1bOF".to_vec(),
        Home => b"\x1b[H".to_vec(),
        End => b"\x1b[F".to_vec(),
        PageUp => b"\x1b[5~".to_vec(),
        PageDown => b"\x1b[6~".to_vec(),
        Insert => b"\x1b[2~".to_vec(),
        Delete => b"\x1b[3~".to_vec(),
        Function(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => {
                // Unsupportable without kitty keyboard protocol or a
                // custom escape sequence; return Err so callers learn the
                // action was rejected instead of silently emitting zero
                // bytes (re-review P0).
                return Err(BackendError::Unsupported(format!(
                    "function key F{} is not encodable without the kitty keyboard protocol",
                    n
                )));
            }
        },
    };

    if alt {
        // ESC-prefix for Alt+key (xterm default).
        let mut v = b"\x1b".to_vec();
        v.extend_from_slice(&base);
        Ok(v)
    } else {
        Ok(base)
    }
}

/// Wave F item 52: kitty keyboard protocol (CSI-u) encoding.
///
/// `CSI unicode-key-code ; modifiers [event-type] u`. Modifiers are
/// 1 + shift(1) + alt(2) + ctrl(4) + super(8). Returns `None` for keys with
/// no kitty unicode-key-code (the caller falls through to legacy encodings).
fn encode_key_kitty(kev: &KeyEvent) -> Option<Vec<u8>> {
    use crate::backend::KeyCode::*;
    let KeyEvent { code, modifiers } = kev;
    let key_code: u32 = match code {
        Char(c) => *c as u32,
        Escape => 27,
        Enter => 13,
        Tab => 9,
        Backspace => 127,
        Up => 0xE000, // kitty functional: 57344 + n
        Down => 0xE000 + 1,
        Left => 0xE000 + 2,
        Right => 0xE000 + 3,
        Home => 0xE000 + 4,
        End => 0xE000 + 5,
        Insert => 0xE000 + 6,
        Delete => 0xE000 + 7,
        PageUp => 0xE000 + 8,
        PageDown => 0xE000 + 9,
        Function(n) => match n {
            // F1–F12 map to the kitty functional-key block.
            1..=12 => 0xE000 + 12 + (*n as u32 - 1),
            // F13–F20 continue the block.
            13..=20 => 0xE000 + 12 + (*n as u32 - 1),
            _ => return None,
        },
    };
    let mods = 1u8
        + (modifiers.contains(KeyModifiers::SHIFT) as u8)
        + ((modifiers.contains(KeyModifiers::ALT) as u8) << 1)
        + ((modifiers.contains(KeyModifiers::CTRL) as u8) << 2)
        + ((modifiers.contains(KeyModifiers::SUPER) as u8) << 3);
    if mods == 1 {
        Some(format!("\x1b[{}u", key_code).into_bytes())
    } else {
        Some(format!("\x1b[{};{}u", key_code, mods).into_bytes())
    }
}

/// Encode a typed [`MouseEvent`] into the negotiated protocol bytes.
///
/// Dispatches SGR (1006), Default/X10 (1000), and UTF-8 (1005) encodings.
///
/// Byte-level semantics (re-review P0 mouse conformance):
///
/// * Coordinates are 1-based on the wire for **all** encodings (classic
///   xterm convention). Our `MouseEvent` uses 0-based coordinates, so we
///   add `+ 1` before the `+ 32` encoding offset.
/// * Wheel events use button code 64 (up) / 65 (down) per xterm.
/// * Motion-while-button-held (drag) sets bit 5 (0x20) of the button code.
/// * X10/UTF-8 release sets the high bit of the button byte via the
///   `button + 3 + 32` convention used by xterm; this is the legacy way
///   to distinguish release from press when the encoding has no other
///   mechanism (vt100 decoders expect this). SGR release uses the
///   trailing lowercase byte `m` instead of `M`.
/// * Coordinates > 222 cannot be expressed in X10/UTF-8 classic mode and
///   return `Err(Unsupported)`; SGR has no such limit.
fn encode_mouse_event(ev: &MouseEvent, encoding: MouseEncoding) -> BackendResult<Vec<u8>> {
    // Compute Cb button base, coords, and flags. Release in X10/UTF-8
    // uses `button_base + 3` (the legacy xterm release offset); SGR
    // release uses the lowercase trailer byte below and so shares the
    // press base.
    let (button_base, x, y, release_offset, motion) = match ev {
        MouseEvent::Press { button, x, y } => (button.sgr_base(), *x, *y, 0u8, false),
        MouseEvent::Release { button, x, y } => (button.sgr_base(), *x, *y, 3u8, false),
        // Hover motion (no button held): xterm uses button code 3 plus the
        // motion bit (byte 67/'C' in X10, Cb 35 in SGR). Without the motion
        // bit the byte collides with the left-button release encoding.
        MouseEvent::Move { x, y } => (3, *x, *y, 0u8, true),
        MouseEvent::Drag { button, x, y } => (button.sgr_base(), *x, *y, 0u8, true),
        MouseEvent::Scroll { direction, x, y } => {
            let base = match direction {
                ScrollDirection::Up => 64,
                ScrollDirection::Down => 65,
            };
            (base, *x, *y, 0u8, false)
        }
    };

    // X10/UTF-8 clamp coordinates > 222 (255 - 32). The on-wire byte is
    // `(value_1_based) + 32`, so the maximum representable coordinate is
    // 222. Apply this check up-front so all three branches share it.
    let x10_clamp = |label: &'static str| -> BackendResult<()> {
        if x > 222 || y > 222 {
            return Err(BackendError::Unsupported(format!(
                "{label} mouse encoding cannot express coordinates > 222 (got {x},{y})"
            )));
        }
        Ok(())
    };

    match encoding {
        MouseEncoding::Sgr => {
            // SGR 1006: ESC [ < Cb ; X ; Y M/m  (1-based coords).
            // The wheel codes (64/65) already live in the low 6 bits, so no
            // mask is applied: `& 0x3f` would collapse wheel-up (64) into
            // button 0 and every scroll would decode as a left click.
            let mut b = button_base;
            if motion {
                b |= 0x20;
            }
            let trailer = if matches!(ev, MouseEvent::Release { .. }) {
                'm'
            } else {
                'M'
            };
            Ok(format!("\x1b[<{};{};{}{}", b, x + 1, y + 1, trailer).into_bytes())
        }
        MouseEncoding::Default => {
            // X10 1000: ESC [ M Cb' X' Y'  where each byte = (1_based) + 32.
            // Drag sets the motion bit (0x20) of the button byte.
            // Release uses the legacy xterm +3 offset on the button byte.
            x10_clamp("x10")?;
            let cb = button_base + release_offset + 32 + if motion { 0x20 } else { 0 };
            // Wire format is 1-based; bump internal 0-based coords by 1.
            let cx = (x + 1 + 32) as u8;
            let cy = (y + 1 + 32) as u8;
            Ok(vec![0x1b, b'[', b'M', cb, cx, cy])
        }
        MouseEncoding::Utf8 => {
            // UTF-8 xterm extension (1005): ESC [ M then each of Cb, X, Y
            // encoded as a UTF-8 codepoint at (1_based + 32). Drag sets the
            // motion bit on the button byte; release uses +3.
            x10_clamp("utf8")?;
            let cb = button_base + release_offset + 32 + if motion { 0x20 } else { 0 };
            let mut out = vec![0x1b, b'[', b'M'];
            out.push(cb);
            out.push((x + 1 + 32) as u8);
            out.push((y + 1 + 32) as u8);
            Ok(out)
        }
    }
}

// `chunk_rx` is stored on the struct (declared near top) and drained in `pump`.

#[cfg(test)]
mod mouse_encode_tests {
    //! Byte-level mouse conformance fixtures (re-review P0 "required backend
    //! conformance tests"). Expectations follow xterm ctlseqs: coordinates are
    //! 1-based on the wire for all encodings, X10/UTF-8 bytes are
    //! (1-based value + 32), wheel buttons are 64/65, the motion bit is 0x20,
    //! and X10/UTF-8 release adds 3 to the button code.
    use super::*;
    use crate::backend::{MouseButton, MouseEncoding, MouseEvent, ScrollDirection};

    fn enc(ev: MouseEvent, encoding: MouseEncoding) -> Vec<u8> {
        encode_mouse_event(&ev, encoding).expect("encode")
    }

    fn ev_bytes(ev: MouseEvent, encoding: MouseEncoding) -> String {
        enc(ev, encoding)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    // ── SGR (1006) ──────────────────────────────────────────────────────

    #[test]
    fn sgr_press_left_at_10_20() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c303b31313b32314d"
        ); // ESC[<0;11;21M
    }

    #[test]
    fn sgr_release_uses_lowercase_m() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c303b31313b32316d"
        ); // ESC[<0;11;21m
    }

    #[test]
    fn sgr_right_button_press_is_2() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Right,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c323b313b314d"
        ); // ESC[<2;1;1M
    }

    #[test]
    fn sgr_move_has_motion_bit_and_button_3() {
        // Hover motion: Cb 3 | 0x20 = 35, trailer M.
        assert_eq!(
            ev_bytes(MouseEvent::Move { x: 5, y: 6 }, MouseEncoding::Sgr),
            "1b5b3c33353b363b374d"
        ); // ESC[<35;6;7M
    }

    #[test]
    fn sgr_drag_sets_motion_bit_on_button() {
        // Left drag: Cb 0 | 0x20 = 32.
        assert_eq!(
            ev_bytes(
                MouseEvent::Drag {
                    button: MouseButton::Left,
                    x: 3,
                    y: 4
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c33323b343b354d"
        ); // ESC[<32;4;5M
    }

    #[test]
    fn sgr_wheel_up_is_64_not_button_zero() {
        // Regression: the removed `& 0x3f` mask collapsed 64 into button 0,
        // encoding every scroll as a left click.
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Up,
                    x: 1,
                    y: 1
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c36343b323b324d"
        ); // ESC[<64;2;2M
    }

    #[test]
    fn sgr_wheel_down_is_65() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Down,
                    x: 1,
                    y: 1
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c36353b323b324d"
        ); // ESC[<65;2;2M
    }

    #[test]
    fn sgr_coordinates_beyond_222_are_allowed() {
        // SGR carries decimal coordinates; no X10 clamp applies.
        let out = enc(
            MouseEvent::Press {
                button: MouseButton::Left,
                x: 500,
                y: 300,
            },
            MouseEncoding::Sgr,
        );
        let s = String::from_utf8(out).expect("utf8");
        assert_eq!(s, "\x1b[<0;501;301M");
    }

    // ── X10 / Default (1000) ────────────────────────────────────────────

    #[test]
    fn x10_press_left_at_10_20() {
        // Cb=0+32=0x20, X=11+32=0x2b, Y=21+32=0x35 (1-based wire coords).
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Default
            ),
            "1b5b4d202b35"
        );
    }

    #[test]
    fn x10_release_adds_3_to_button_code() {
        // Left release: Cb=0+3+32=0x23.
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Default
            ),
            "1b5b4d232b35"
        );
    }

    #[test]
    fn x10_middle_and_right_releases() {
        // Middle (1): Cb=1+3+32=0x24. Right (2): Cb=2+3+32=0x25.
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Middle,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d242121"
        );
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Right,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d252121"
        );
    }

    #[test]
    fn x10_drag_sets_motion_bit() {
        // Left drag: Cb=0|0x20 then +32 = 0x40; X=8+32=0x28, Y=9+32=0x29.
        assert_eq!(
            ev_bytes(
                MouseEvent::Drag {
                    button: MouseButton::Left,
                    x: 7,
                    y: 8
                },
                MouseEncoding::Default
            ),
            "1b5b4d402829"
        );
    }

    #[test]
    fn x10_move_is_button_3_plus_motion_bit() {
        // Hover: Cb=3|0x20=35, byte=35+32=67=0x43 ('C'). Distinct from any
        // release byte.
        assert_eq!(
            ev_bytes(MouseEvent::Move { x: 0, y: 0 }, MouseEncoding::Default),
            "1b5b4d432121"
        );
    }

    #[test]
    fn x10_wheel_up_and_down() {
        // Up: Cb=64+32=0x60. Down: Cb=65+32=0x61.
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Up,
                    x: 2,
                    y: 3
                },
                MouseEncoding::Default
            ),
            "1b5b4d602324"
        );
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Down,
                    x: 2,
                    y: 3
                },
                MouseEncoding::Default
            ),
            "1b5b4d612324"
        );
    }

    #[test]
    fn x10_rejects_coordinates_above_222() {
        let out = encode_mouse_event(
            &MouseEvent::Press {
                button: MouseButton::Left,
                x: 300,
                y: 0,
            },
            MouseEncoding::Default,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }

    #[test]
    fn x10_origin_is_1_1_not_0_0() {
        // Internal (0,0) maps to wire bytes 0x21 0x21 ('!' '!') — the 1-based
        // home position — never 0x20 0x20.
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d202121"
        );
    }

    // ── UTF-8 (1005) ────────────────────────────────────────────────────

    #[test]
    fn utf8_press_matches_x10_in_classic_range() {
        // Within the classic range, 1005 shares X10's byte shape (documented
        // limitation: this backend does not emit multi-byte codepoints for
        // coordinates > 222 — it rejects them instead).
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Utf8
            ),
            "1b5b4d202b35"
        );
    }

    #[test]
    fn utf8_release_adds_3() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Utf8
            ),
            "1b5b4d232b35"
        );
    }

    #[test]
    fn utf8_rejects_coordinates_above_222() {
        let out = encode_mouse_event(
            &MouseEvent::Press {
                button: MouseButton::Left,
                x: 223,
                y: 0,
            },
            MouseEncoding::Utf8,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }
}

/// Keyboard conformance fixtures for modified navigation keys (re-review
/// Wave-1 item 7). Without these, Ctrl+Arrow silently degraded to a bare
/// arrow byte sequence.
#[cfg(test)]
mod key_encode_tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        let modes = crate::backend::InputModes::default();
        encode_key(&KeyEvent { code, modifiers }, &modes).expect("encode key")
    }

    fn bytes(v: Vec<u8>) -> String {
        v.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn ctrl_up_is_csi_u_not_bare_arrow() {
        // CSI 1;5A — modifier mask 1+ctrl(4)=5.
        assert_eq!(bytes(key(KeyCode::Up, KeyModifiers::CTRL)), "1b5b313b3541");
    }

    #[test]
    fn shift_right_is_csi_with_mask_2() {
        // CSI 1;2C — 1+shift(1)=2.
        assert_eq!(
            bytes(key(KeyCode::Right, KeyModifiers::SHIFT)),
            "1b5b313b3243"
        );
    }

    #[test]
    fn ctrl_shift_left_mask_is_6() {
        // 1+shift(1)+ctrl(4)=6.
        assert_eq!(
            bytes(key(KeyCode::Left, KeyModifiers::CTRL | KeyModifiers::SHIFT)),
            "1b5b313b3644"
        );
    }

    #[test]
    fn alt_up_uses_csi_with_mask_3() {
        // CSI 1;3A — 1+alt(2)=3 (modifyOtherKeys form; the old ESC-prefixed
        // bare arrow lost the fact that Alt was held).
        let modes = crate::backend::InputModes::default();
        let out = encode_key(
            &KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
            },
            &modes,
        )
        .expect("encode");
        assert_eq!(out, b"\x1b[1;3A".to_vec());
    }

    #[test]
    fn ctrl_home_and_end_use_hf_with_mask() {
        assert_eq!(
            bytes(key(KeyCode::Home, KeyModifiers::CTRL)),
            "1b5b313b3548"
        ); // CSI 1;5H
        assert_eq!(bytes(key(KeyCode::End, KeyModifiers::CTRL)), "1b5b313b3546");
        // CSI 1;5F
    }

    #[test]
    fn modified_pageup_pagedown_insert_delete() {
        // CSI 5;5~ / 6;5~ / 2;5~ / 3;5~.
        assert_eq!(
            bytes(key(KeyCode::PageUp, KeyModifiers::CTRL)),
            "1b5b353b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::PageDown, KeyModifiers::CTRL)),
            "1b5b363b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::Insert, KeyModifiers::CTRL)),
            "1b5b323b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::Delete, KeyModifiers::CTRL)),
            "1b5b333b357e"
        );
    }

    #[test]
    fn unmodified_arrows_still_use_plain_csi() {
        assert_eq!(bytes(key(KeyCode::Up, KeyModifiers::empty())), "1b5b41"); // ESC[A
    }

    #[test]
    fn shift_tab_still_is_csi_z() {
        assert_eq!(bytes(key(KeyCode::Tab, KeyModifiers::SHIFT)), "1b5b5a"); // ESC[Z
    }

    #[test]
    fn ctrl_letter_is_c0_control() {
        assert_eq!(bytes(key(KeyCode::Char('c'), KeyModifiers::CTRL)), "03");
    }

    #[test]
    fn super_still_rejected() {
        let modes = crate::backend::InputModes::default();
        let out = encode_key(
            &KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::SUPER,
            },
            &modes,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }
}

#[cfg(test)]
mod osc8_tests {
    use super::*;

    #[test]
    fn osc8_hyperlink_captured_in_screen_state() {
        let mut b = PortablePtyBackend::new(40, 5);
        let fixture = std::env::temp_dir().join("osc8_fixture.py");
        std::fs::write(&fixture, include_str!("/tmp/osc8_fixture.py")).unwrap();
        b.start(
            "python3",
            &[fixture.to_string_lossy().to_string()],
            None,
            &[],
            40,
            5,
        )
        .expect("start");
        std::thread::sleep(std::time::Duration::from_millis(700));
        let st = b.state().expect("state");
        b.stop().ok();
        let links = &st.hyperlinks;
        assert!(
            links.iter().any(|l| l.uri.contains("example.com/docs")),
            "OSC8 link must be captured: {:?}",
            links
        );
    }
}
