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
}

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
    // Event sequencing state (spec section 1).
    output_seq: u64,
    screen_seq: u64,
    content_seq: u64,
    bell_seq: u64,
    title_seq: u64,
    last_output_at_ms: u64,
    last_screen_change_at_ms: u64,
    // Monotonic Instants for fine-grained wait timing (monotonic clock,
    // unlike SystemTime which can jump). These are updated in `pump()`
    // whenever the corresponding event occurs.
    child_pid: Option<u32>,
    last_output_instant: Instant,
    last_screen_change_instant: Instant,
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
            output_seq: 0,
            screen_seq: 0,
            content_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            last_output_at_ms: 0,
            last_screen_change_at_ms: 0,
            child_pid: None,
            last_output_instant: now,
            last_screen_change_instant: now,
        }
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
        if let Some(rx) = self.chunk_rx.as_ref() {
            // Non-blocking drain of everything currently buffered.
            while let Ok(chunk) = rx.try_recv() {
                self.parser.process(&chunk);
                self.output_seq += 1;
                self.last_output_at_ms = now_ms();
                self.last_output_instant = Instant::now();
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
        }
        Ok((before_fp, after_fp))
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
}

impl TerminalBackend for PortablePtyBackend {
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
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
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
        self.last_output_at_ms = 0;
        self.last_screen_change_at_ms = 0;
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
        Ok(crate::screen::from_vt(self.parser.screen(), process, title))
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
        let baseline_bell_seq = self.bell_seq;

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
            let screen = crate::screen::from_vt(self.parser.screen(), process, title);

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
                WaitCond::Bell => (self.bell_seq > baseline_bell_seq, WaitReason::Bell),
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
        let mut caps = Capabilities::honest();
        // Promote optional capabilities only when we have observed the running
        // application negotiate them.
        caps.title = self.title_seq > 0;
        caps.scrollback = false; // vt100 scrollback not yet surfaced (spec 17)
        caps.mouse = self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        caps.bracketed_paste = self.parser.screen().bracketed_paste();
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
        }
    }

    /// Attach or detach the raw PTY recording hook (audit item 24).
    /// Stores the slot so the struct can forward on_input/on_resize events.
    fn set_recording_hook(&mut self, hook: super::RecordingHookSlot) {
        self.recording_slot = hook;
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
/// SUPER is rejected (unsupported without kitty keyboard protocol)
/// (audit item 6).
fn encode_key(kev: &KeyEvent, modes: &InputModes) -> BackendResult<Vec<u8>> {
    use crate::backend::KeyCode::*;
    let KeyEvent { code, modifiers } = kev;

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
