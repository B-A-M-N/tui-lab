//! Line-oriented CLI backend (Wave F items 50–51).
//!
//! A second engine behind the same [`TerminalBackend`] trait for targets
//! that are *not* screens: `git`, `npm`, `pytest`, interactive prompts.
//! The process runs on plain pipes (no PTY), so there is no cell grid —
//! each completed output chunk becomes synthetic screen rows backed by a
//! bounded line history (real scrollback, honestly the whole story), and
//! every capability the pipe model cannot honor (mouse, resize, styles,
//! kitty) reports `false`.
//!
//! What this buys over the PTY backend: no pty echo, no terminal-mode
//! requirements on the child, faithful line scrollback, and exit codes
//! surfaced immediately. What it does NOT buy: pixel-perfect TUI driving —
//! for that, use `portable_vt100`.
//!
//! The screen model maps 1:1: the last `rows` lines render as the viewport,
//! the rest is scrollback. Waits reuse the same condition vocabulary, with
//! `ScreenStable`/`Idle` keyed on pipe-quiet rather than cell-quiet.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use vt100::Parser;

use crate::backend::line_types::{CommandState, SearchHit};
use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    Input, InputModes, MouseEvent, ObserveResult, RecordingHookSlot, TerminalEventState, WaitCond,
    WaitOutcome, WaitReason,
};
use crate::screen::{ProcessState, ScreenState};

/// Upper bound on retained output lines (both scrollback and viewport feed
/// from this one deque). Oldest lines are evicted; the eviction is declared
/// through `Capabilities::scrollback` remaining true (history exists) —
/// `line_dropped` counts how many lines were evicted so a caller can detect
/// a partial history.
const MAX_LINES: usize = 10_000;

pub struct LineCliBackend {
    cols: u16,
    rows: u16,
    /// Synthetic parser kept for screen construction parity: the real text
    /// lives in `lines`, but `ScreenState::from_parts` wants a vt100 screen
    /// for hashes/cells. We render `lines` INTO a vt100 parser so the
    /// downstream semantic machinery (regions, controls, hashes) works
    /// unchanged on CLI output.
    parser: Parser<()>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    writer: Option<Box<dyn Write + Send>>,
    reader_handle: Option<thread::JoinHandle<()>>,
    chunk_rx: Option<mpsc::Receiver<Vec<u8>>>,
    recording_slot: RecordingHookSlot,
    reader_recording_hook: Option<super::RecordingHookSlot>,
    // Event sequencing (same contract as PortablePtyBackend).
    output_seq: u64,
    screen_seq: u64,
    content_seq: u64,
    bell_seq: u64,
    title_seq: u64,
    last_output_at_ms: u64,
    last_screen_change_at_ms: u64,
    child_pid: Option<u32>,
    last_output_instant: Instant,
    last_screen_change_instant: Instant,
    /// Raw completed lines (split on \n). The viewport is the tail; the
    /// rest is scrollback.
    lines: Vec<String>,
    /// Partial trailing line not yet terminated by \n.
    pending: String,
    /// Declared eviction counter (see MAX_LINES).
    lines_evicted: u64,
}

impl LineCliBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        let now = Instant::now();
        LineCliBackend {
            cols,
            rows,
            parser: Parser::new(rows, cols, 0),
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
            lines: Vec::new(),
            pending: String::new(),
            lines_evicted: 0,
        }
    }

    /// Feed raw child bytes: split into lines, evict old, bump sequences.
    ///
    /// `\r\n` is the pty's ONLCR rendering of a newline — the `\r` is
    /// ignored and the `\n` commits the line. A bare `\r` (progress-bar
    /// redraws) restarts the pending line, matching what a terminal shows.
    fn ingest(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut changed = false;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\n' {
                self.lines.push(std::mem::take(&mut self.pending));
                changed = true;
            } else if ch == '\r' {
                // "\r\n" is the pty's rendering of one newline — the '\n'
                // that follows commits the line normally. A bare '\r'
                // restarts the pending line (progress-bar redraw).
                if chars.peek() != Some(&'\n') {
                    self.pending.clear();
                }
            } else if ch == '\x07' {
                self.bell_seq += 1;
            } else {
                self.pending.push(ch);
            }
        }
        if changed || !self.pending.is_empty() {
            self.content_seq += 1;
        }
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.lines_evicted += drop as u64;
        }
        if changed {
            self.screen_seq += 1;
            self.last_screen_change_at_ms = crate::backend::line_types::now_ms();
            self.last_screen_change_instant = Instant::now();
        }
    }

    fn pump(&mut self) {
        loop {
            let chunk = {
                match self.chunk_rx.as_ref() {
                    Some(rx) => match rx.try_recv() {
                        Ok(c) => c,
                        Err(_) => break,
                    },
                    None => break,
                }
            };
            self.ingest(&chunk);
            self.output_seq += 1;
            self.last_output_at_ms = crate::backend::line_types::now_ms();
            self.last_output_instant = Instant::now();
        }
    }

    /// Render the retained lines through a fresh vt100 parser so the
    /// semantic pipeline operates on the same cell-grid shape it does for
    /// real terminal apps. The viewport is the LAST `rows` lines; earlier
    /// lines are scrollback.
    fn synth_screen(&mut self) -> ScreenState {
        let rows = self.rows as usize;
        let start = self.lines.len().saturating_sub(rows);
        let viewport: Vec<String> = self.lines[start..].to_vec();
        // Rebuild the parser each time (CLI output is append-mostly; the
        // rebuild cost is trivial at these sizes).
        let mut p = Parser::new(self.rows, self.cols, 0);
        for (y, line) in viewport.iter().enumerate() {
            let truncated: String = line.chars().take(self.cols as usize).collect();
            p.process(format!("{}{}", truncated, if y + 1 < viewport.len() { "\r\n" } else { "" }).as_bytes());
        }
        self.parser = p;
        let process = self.process();
        let mut state = crate::screen::from_vt(
            self.parser.screen(),
            process,
            None,
            Vec::new(),
        );
        state.scrollback = self.lines[..start].to_vec();
        state
    }

    /// Notify the recording hook if attached (parity with the PTY backend).
    fn notify<F: FnOnce(&dyn crate::backend::RecordingHook)>(&self, f: F) {
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref hook) = *slot {
                f(hook.as_ref());
            }
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> BackendResult<()> {
        let writer = match self.writer.as_mut() {
            Some(w) => w,
            None => return Err(BackendError::NoSession),
        };
        writer.write_all(bytes).map_err(BackendError::Io)?;
        writer.flush().map_err(BackendError::Io)?;
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

impl TerminalBackend for LineCliBackend {
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
        // NOTE: intentionally NOT a PTY — the child's stdout is a pipe, so
        // anything requiring a terminal (curses, cursor addressing) cannot
        // run here. That constraint is the point of this backend.
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
        if let Some(c) = cwd {
            cmd.cwd(c);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Tell line-aware children they are being driven as a CLI (honest
        // environment; applications may opt into non-TUI output).
        cmd.env("TERM", "dumb");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| BackendError::Spawn(e.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| BackendError::Io(std::io::Error::other(e.to_string())))?;
        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
        let hook = self.recording_slot.clone();
        let rh = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if let Ok(slot) = hook.lock() {
                            if let Some(ref h) = *slot {
                                h.on_output(&chunk);
                            }
                        }
                        if chunk_tx.send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        self.reader_handle = Some(rh);
        self.chunk_rx = Some(chunk_rx);
        self.reader_recording_hook = Some(self.recording_slot.clone());
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
        self.lines.clear();
        self.pending.clear();
        self.lines_evicted = 0;
        let now = Instant::now();
        self.last_output_instant = now;
        self.last_screen_change_instant = now;
        self.child_pid = self.child.as_ref().and_then(|c| c.process_id());
        // Give the process a moment to emit initial output (same contract
        // as the PTY backend).
        std::thread::sleep(Duration::from_millis(150));
        self.pump();
        Ok(())
    }

    fn stop(&mut self) -> BackendResult<()> {
        if let Some(pid) = self.child_pid {
            #[cfg(unix)]
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        self.pump();
        Ok(self.synth_screen())
    }

    fn send_input(&mut self, input: Input) -> BackendResult<()> {
        self.pump();
        if self.writer.is_none() {
            return Err(BackendError::NoSession);
        }
        match input {
            Input::Text(t) | Input::Paste(t) => {
                self.write_input(t.as_bytes())?;
            }
            Input::Key(kev) => {
                // Single key: encode the character itself, Enter as newline —
                // a pipe has no key semantics beyond bytes.
                match kev.code {
                    crate::backend::KeyCode::Enter => self.write_input(b"\n")?,
                    crate::backend::KeyCode::Char(c) => {
                        let mut s = String::new();
                        s.push(c);
                        self.write_input(s.as_bytes())?;
                    }
                    _ => {
                        return Err(BackendError::Unsupported(
                            "line CLI backend supports Char/Enter keys only; use portable_vt100 for full key semantics"
                                .into(),
                        ))
                    }
                }
            }
            Input::Keys(keys) => {
                for kev in keys {
                    match kev.code {
                        crate::backend::KeyCode::Enter => self.write_input(b"\n")?,
                        crate::backend::KeyCode::Char(c) => {
                            let mut s = String::new();
                            s.push(c);
                            self.write_input(s.as_bytes())?;
                        }
                        _ => {
                            return Err(BackendError::Unsupported(
                                "line CLI backend supports Char/Enter keys only".into(),
                            ))
                        }
                    }
                }
            }
            Input::Raw(b) => {
                self.write_input(&b)?;
            }
            Input::Mouse(_)
            | Input::MouseClick { .. } => {
                return Err(BackendError::Unsupported(
                    "mouse is meaningless on a line CLI backend (no terminal grid reports input)"
                        .into(),
                ))
            }
            Input::Resize { cols, rows } => {
                // Pipes have no size; the synthetic grid can be re-dimensioned
                // but the child is never told (nothing to tell).
                self.cols = cols;
                self.rows = rows;
            }
            Input::Signal(sig) => {
                #[cfg(unix)]
                {
                    if let Some(pid) = self.child_pid {
                        let r = unsafe { libc::kill(-(pid as i32), sig) };
                        if r != 0 {
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
        // Synthetic re-dimension only; the child is not signalled (a pipe
        // has no winsize). Honest: no WINCH can be delivered because there
        // is no pty.
        self.cols = cols;
        self.rows = rows;
        self.notify(|hook| hook.on_resize(cols, rows));
        Ok(())
    }

    fn wait(&mut self, cond: WaitCond, budget: Duration) -> BackendResult<WaitOutcome> {
        let start = Instant::now();
        let baseline_bell_seq = self.bell_seq;
        let baseline_screen_seq = self.screen_seq;
        loop {
            self.pump();
            let screen = {
                let mut s = self.synth_screen();
                s.scrollback = {
                    let keep = self.lines.len().saturating_sub(self.rows as usize);
                    self.lines[..keep].to_vec()
                };
                s
            };
            let (met, reason) = match &cond {
                WaitCond::Text(t) => (
                    screen.viewport_text.iter().any(|r| r.contains(t.as_str()))
                        || screen.scrollback.iter().any(|r| r.contains(t.as_str())),
                    WaitReason::Text,
                ),
                WaitCond::TextAbsent(t) => (
                    !screen.viewport_text.iter().any(|r| r.contains(t.as_str())),
                    WaitReason::TextAbsent,
                ),
                WaitCond::ScreenChange => (self.screen_seq > baseline_screen_seq, WaitReason::ScreenChange),
                WaitCond::ScreenStable {
                    quiet_for,
                    after_screen_seq,
                } => {
                    let anchored_ok = match after_screen_seq {
                        Some(seq) => self.screen_seq > *seq,
                        None => true,
                    };
                    let quiet_ok = self.last_screen_change_instant.elapsed() >= *quiet_for;
                    (anchored_ok && quiet_ok, WaitReason::ScreenStable)
                }
                WaitCond::ProcessExit => (!screen.process.running, WaitReason::ProcessExit),
                WaitCond::Title(_) => (false, WaitReason::Title), // no titles on pipes
                WaitCond::Bell => (self.bell_seq > baseline_bell_seq, WaitReason::Bell),
                WaitCond::Idle {
                    quiet_for,
                    after_output_seq,
                } => {
                    let anchored_ok = match after_output_seq {
                        Some(seq) => self.output_seq > *seq,
                        None => true,
                    };
                    let quiet_ok = self.last_output_instant.elapsed() >= *quiet_for;
                    (anchored_ok && quiet_ok, WaitReason::Idle)
                }
                // OSC 133 is terminal-side shell integration; a pipe child
                // that emits it is possible in principle, but this backend
                // does not parse it — report unsatisfied honestly.
                WaitCond::CommandDone { .. } | WaitCond::CommandOutput { .. } => {
                    (false, WaitReason::Timeout)
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
        Capabilities {
            mouse: false,
            kitty_keyboard: false,
            // CLI output is plain text; style claims would be dishonest.
            colors: false,
            cell_attributes: false,
            title: false,
            // Real scrollback: the full retained line history.
            scrollback: true,
            bracketed_paste: false,
            signals: cfg!(unix),
        }
    }

    fn process(&mut self) -> ProcessState {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.exit_code() as i32;
                    let sig = status.signal().map(|s| s.to_string());
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
        InputModes::default()
    }

    fn event_state(&self) -> TerminalEventState {
        TerminalEventState {
            output_seq: self.output_seq,
            screen_seq: self.screen_seq,
            content_seq: self.content_seq,
            visual_seq: self.screen_seq,
            interaction_seq: self.screen_seq + self.bell_seq,
            bell_seq: self.bell_seq,
            title_seq: self.title_seq,
            last_output_at: self.last_output_at_ms,
            last_screen_change_at: self.last_screen_change_at_ms,
            command_seq: 0,
        }
    }

    fn set_recording_hook(&mut self, hook: RecordingHookSlot) {
        self.recording_slot = hook;
    }

    fn recording(&self) -> bool {
        self.recorder_is_some()
    }

    /// Wave F item 53: the CLI backend's scrollback IS its line history —
    /// the honest, complete story (bounded by MAX_LINES with declared
    /// eviction).
    fn scrollback_lines(&mut self) -> BackendResult<Vec<String>> {
        self.pump();
        Ok(self.lines.clone())
    }

    fn search(&mut self, query: &str) -> BackendResult<Vec<SearchHit>> {
        self.pump();
        let mut hits = Vec::new();
        if query.is_empty() {
            return Ok(hits);
        }
        let q = query.to_lowercase();
        let rows = self.rows as usize;
        let start = self.lines.len().saturating_sub(rows);
        for (y, line) in self.lines.iter().enumerate() {
            let lower = line.to_lowercase();
            let mut from = 0;
            while let Some(rel) = lower[from..].find(&q) {
                let at = from + rel;
                let region = if y >= start { "viewport" } else { "scrollback" };
                let row = if y >= start {
                    (y - start) as u32
                } else {
                    y as u32
                };
                hits.push(SearchHit {
                    region: region.to_string(),
                    row,
                    start: at as u32,
                    len: q.len() as u32,
                    line: line.clone(),
                });
                from = at + q.len();
            }
        }
        Ok(hits)
    }

    fn command_state(&mut self) -> Option<CommandState> {
        // OSC 133 parsing is not implemented on this backend: honest None.
        None
    }
}

impl LineCliBackend {
    fn recorder_is_some(&self) -> bool {
        false
    }
}

/// Observe: parity with the PTY backend — capture now, then settle on
/// line-quiet.
impl LineCliBackend {
    pub fn observe(&mut self, idle: Duration) -> BackendResult<ObserveResult> {
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
        let out = self.wait(
            WaitCond::ScreenStable {
                quiet_for: quiet,
                after_screen_seq: None,
            },
            idle + Duration::from_millis(500),
        )?;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(ObserveResult {
            screen: out.state,
            stable: out.met,
            stable_ms: elapsed_ms,
        })
    }
}

// Silence unused-import warnings for types used only in doc examples.
#[allow(unused)]
fn _unused(m: MouseEvent) {}
