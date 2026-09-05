//! True pipe backend (review P0: genuine pipes, no PTY).
//!
//! The third engine behind the same [`TerminalBackend`] trait. Unlike
//! [`super::portable_pty::PortablePtyBackend`] (a real PTY + vt100 grid) and
//! [`super::line_cli::PtyLineBackend`] (a real PTY interpreted line-by-line),
//! this backend launches the child with **stdin/stdout/stderr all connected
//! to pipes** — `isatty()` is `false` for the child, so programs that demand
//! a terminal fail exactly as they would under a real pipe redirect. This is
//! the honest "run `<cmd> | …`" transport the review asked for.
//!
//! What this buys: genuine `stdout`/`stderr` separation (review P1 #28) and
//! byte-exact capture with zero terminal emulation bias. What it does NOT
//! buy: keyboard/TUI driving — there is no terminal grid, so mouse and full
//! key semantics are honestly rejected, and programs requiring `isatty()`
//! will exit rather than pretend.
//!
//! The screen model mirrors the line backend: the stream is split on `\n`
//! into a bounded line history; the last `rows` lines render as the viewport
//! and the rest is scrollback, with an unterminated `pending` line as the
//! live current line. Both streams feed the fused screen in arrival order
//! (as a terminal would interleave them), while the raw `stdout`/`stderr`
//! line stores stay separable so a caller can read STDERR distinctly.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vt100::Parser;

use crate::backend::line_types::{now_ms, CommandState, SearchHit};
use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    Input, InputModes, ObserveResult, RecordingHookSlot, TerminalEventState, WaitCond, WaitOutcome,
    WaitReason,
};
use crate::screen::{ProcessState, ScreenState};

/// Upper bound on retained output lines per stream (the fused `lines` deque
/// is bounded by `MAX_LINES`; eviction is declared through the capability
/// staying true while `lines_evicted` counts how many were dropped).
const MAX_LINES: usize = 10_000;

/// A byte stream read off a pipe and folded into ordered line history.
/// Standard input receives merged bytes; stdout/stderr snapshots keep their
/// raw line stores separate so P1 #28 stderr separation survives.
pub struct PipeBackend {
    cols: u16,
    rows: u16,
    /// Synthetic parser kept for screen construction parity (same rationale
    /// as the line backend): the real text lives in `lines`, but
    /// `ScreenState::from_vt` wants a vt100 screen for hashes/cells, so we
    /// render the fused lines into a parser for the downstream semantic
    /// machinery to work unchanged on pipe output.
    parser: Parser<()>,
    child: Option<Child>,
    /// stdin writer handed to the child.
    stdin: Option<std::process::ChildStdin>,
    stdout_lines: Vec<String>,
    stderr_lines: Vec<String>,
    /// Raw completed lines (fused stdout+stderr in arrival order).
    lines: Vec<String>,
    /// Partial trailing line (fused) not yet terminated by `\n`.
    pending: String,
    /// Declared eviction counter (see MAX_LINES).
    lines_evicted: u64,
    chunk_rx: Option<mpsc::Receiver<Chunk>>,
    reader_handles: Vec<thread::JoinHandle<()>>,
    recording_slot: RecordingHookSlot,
    /// Wave G item 77: clear the inherited env before applying pairs.
    clear_env_on_start: bool,
    /// Per-stream: did the last ingest end inside an unterminated line?
    /// Only that trailing partial continues into the next chunk; committed
    /// lines stay committed (W1b pending-line fix).
    trailing_unterminated: [bool; 2],
    // Event sequencing (same contract as the other backends).
    output_seq: u64,
    screen_seq: u64,
    content_seq: u64,
    bell_seq: u64,
    title_seq: u64,
    last_output_at_ms: u64,
    last_screen_change_at_ms: u64,
    last_output_instant: Instant,
    last_screen_change_instant: Instant,
    child_pid: Option<u32>,
}

/// A byte chunk tagged with its provenance so the fused screen can interleave
/// streams in arrival order while the per-stream stores stay separable.
#[derive(Debug)]
struct Chunk {
    /// Which pipe produced the bytes.
    stream: Stream,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// Index into per-stream bookkeeping (`trailing_unterminated`).
    fn slot(self) -> usize {
        match self {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        }
    }
}

impl PipeBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        let now = Instant::now();
        PipeBackend {
            cols,
            rows,
            parser: Parser::new(rows, cols, 0),
            child: None,
            stdin: None,
            stdout_lines: Vec::new(),
            stderr_lines: Vec::new(),
            lines: Vec::new(),
            pending: String::new(),
            lines_evicted: 0,
            chunk_rx: None,
            reader_handles: Vec::new(),
            recording_slot: new_recording_hook_slot(),
            trailing_unterminated: [false, false],
            clear_env_on_start: false,
            output_seq: 0,
            screen_seq: 0,
            content_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            last_output_at_ms: 0,
            last_screen_change_at_ms: 0,
            last_output_instant: now,
            last_screen_change_instant: now,
            child_pid: None,
        }
    }

    /// Fold one byte into the fused stream: `\n` commits the pending line,
    /// `\a` is a bell edge (nothing printed), everything else appends.
    /// Mirrors the line backend's ingest contract so `screen_seq` advances
    /// on real visible changes — including an unterminated pending mutation.
    fn ingest_fused(&mut self, byte: u8) {
        let mut changed = false; // a line committed to history
        let mut pending_changed = false; // the live pending line mutated
        match byte {
            b'\n' => {
                let mut line = std::mem::take(&mut self.pending);
                if line.len() > self.cols as usize {
                    line.truncate(self.cols as usize);
                }
                self.lines.push(line);
                if self.lines.len() > MAX_LINES {
                    let excess = self.lines.len() - MAX_LINES;
                    self.lines.drain(..excess);
                    self.lines_evicted += excess as u64;
                }
                changed = true;
            }
            b'\r' => {
                if !self.pending.is_empty() {
                    self.pending.clear();
                    pending_changed = true;
                }
            }
            b'\x07' => {
                self.bell_seq += 1;
            }
            c => {
                if self.pending.len() < self.cols as usize {
                    self.pending.push(c as char);
                    pending_changed = true;
                }
            }
        }
        if changed || pending_changed {
            self.content_seq += 1;
            self.screen_seq += 1;
            self.last_screen_change_at_ms = now_ms();
            self.last_screen_change_instant = Instant::now();
        }
    }

    /// Fold one stream's bytes into its own store AND the fused stream.
    fn ingest_stream(&mut self, stream: Stream, bytes: &[u8]) {
        // Per-stream store: only the store's own unterminated trailing line
        // continues — a line that ended with `\n` is committed and the next
        // chunk must start a NEW line. The old unconditional
        // `store.pop().unwrap_or_default()` re-opened committed lines, so
        // "OUT-A\n" and a later chunk "OUT-B\n" fused into one "OUT-AOUT-B"
        // line (W1b). Tracked by whether the last ingest ended mid-line.
        let store = match stream {
            Stream::Stdout => &mut self.stdout_lines,
            Stream::Stderr => &mut self.stderr_lines,
        };
        let mut cur = if self.trailing_unterminated[stream.slot()] {
            store.pop().unwrap_or_default()
        } else {
            String::new()
        };
        self.trailing_unterminated[stream.slot()] = true;
        for &b in bytes {
            match b {
                b'\n' => {
                    if cur.len() > self.cols as usize {
                        cur.truncate(self.cols as usize);
                    }
                    store.push(std::mem::take(&mut cur));
                    self.trailing_unterminated[stream.slot()] = false;
                }
                b'\r' => {}
                b'\x07' => {}
                c => cur.push(c as char),
            }
        }
        if !cur.is_empty() {
            store.push(cur);
        }
        // Bound each separable store like the fused one; declare the eviction
        // on the fused counter (the streams stay internally consistent).
        while store.len() > MAX_LINES {
            store.remove(0);
            self.lines_evicted += 1;
        }
        // Fused ingest: the screen interleaves stdout+stderr in arrival
        // order, stream-agnostic — a pipe consumer sees one output stream.
        for &b in bytes {
            self.ingest_fused(b);
        }
    }

    fn pump(&mut self) {
        // Drain into a local buffer first so `ingest_stream` can borrow `self`
        // mutably without holding `&self.chunk_rx` across the mutation.
        let mut drained: Vec<Chunk> = Vec::new();
        if let Some(rx) = &self.chunk_rx {
            while let Ok(chunk) = rx.try_recv() {
                drained.push(chunk);
            }
        }
        for chunk in drained {
            if !chunk.bytes.is_empty() {
                self.output_seq += 1;
                self.last_output_at_ms = now_ms();
                self.last_output_instant = Instant::now();
                // Notify the recording hook (parity with line backend).
                if let Ok(slot) = self.recording_slot.lock() {
                    if let Some(ref h) = *slot {
                        h.on_output(&chunk.bytes);
                    }
                }
                self.ingest_stream(chunk.stream, &chunk.bytes);
            }
        }
    }

    fn synth_screen(&mut self) -> ScreenState {
        self.pump();
        let rows = self.rows as usize;
        let viewport_rows = if self.pending.is_empty() {
            rows
        } else {
            rows.saturating_sub(1)
        };
        let start = self.lines.len().saturating_sub(viewport_rows);
        let mut viewport_lines: Vec<String> = self.lines[start..].to_vec();
        if !self.pending.is_empty() {
            viewport_lines.push(self.pending.clone());
        }
        let mut p = Parser::new(self.rows, self.cols, 0);
        for (y, line) in viewport_lines.iter().enumerate() {
            let truncated: String = line.chars().take(self.cols as usize).collect();
            p.process(
                format!(
                    "{}{}",
                    truncated,
                    if y + 1 < viewport_lines.len() {
                        "\r\n"
                    } else {
                        ""
                    }
                )
                .as_bytes(),
            );
        }
        self.parser = p;
        let process = self.process();
        let mut state = crate::screen::from_vt(self.parser.screen(), process, None, Vec::new());
        state.scrollback = self.lines[..start].to_vec();
        state
    }

    fn write_input(&mut self, bytes: &[u8]) -> BackendResult<()> {
        let stdin = match self.stdin.as_mut() {
            Some(w) => w,
            None => return Err(BackendError::NoSession),
        };
        stdin.write_all(bytes).map_err(BackendError::Io)?;
        stdin.flush().map_err(BackendError::Io)?;
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref h) = *slot {
                h.on_input(bytes);
            }
        }
        Ok(())
    }

    fn recorder_is_some(&self) -> bool {
        let Ok(slot) = self.recording_slot.lock() else {
            return false;
        };
        slot.is_some()
    }

    /// Genuine stderr separation (review P1 #28): the lines the child wrote
    /// to file descriptor 2, oldest first. Not in the trait — an accessor so
    /// callers can read STDERR distinctly instead of only a merged screen.
    pub fn stderr_lines(&mut self) -> Vec<String> {
        self.pump();
        self.stderr_lines.clone()
    }

    /// Genuine stdout lines (fd 1), oldest first.
    pub fn stdout_lines(&mut self) -> Vec<String> {
        self.pump();
        self.stdout_lines.clone()
    }

    /// Session-facing stream read (Wave-2 streams mode): `(stdout, stderr)`
    /// via the trait's downcast hook. Same data as the two accessors above.
    pub fn stdout_lines_pub(&mut self) -> Vec<String> {
        self.stdout_lines()
    }

    pub fn stderr_lines_pub(&mut self) -> Vec<String> {
        self.stderr_lines()
    }
}

impl TerminalBackend for PipeBackend {
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
        let mut cmd = Command::new(command);
        for a in args {
            cmd.arg(a);
        }
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        // Isolation (Wave G item 77): clean/strict launches drop the inherited
        // environment first.
        if self.clear_env_on_start {
            cmd.env_clear();
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        // A pipe is not a terminal: TERM=dumb so line-aware children produce
        // plain text instead of raw ANSI sequences.
        cmd.env("TERM", "dumb");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| BackendError::Spawn(e.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            BackendError::Spawn("pipe backend could not take stdin handle".into())
        })?;
        let mut stdout_r = child.stdout.take().ok_or_else(|| {
            BackendError::Spawn("pipe backend could not take stdout handle".into())
        })?;
        let mut stderr_r = child.stderr.take().ok_or_else(|| {
            BackendError::Spawn("pipe backend could not take stderr handle".into())
        })?;
        self.child_pid = Some(child.id());

        let (chunk_tx, chunk_rx) = mpsc::channel::<Chunk>();
        // One reader thread per stream (concrete types differ), tagged so the
        // fused interleave is real. The recording hook is attached per chunk
        // on the pump side (parity), so the reader threads stay hook-free.
        let stdout_tx = chunk_tx.clone();
        let mut handles = Vec::new();
        handles.push(thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout_r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        if stdout_tx
                            .send(Chunk {
                                stream: Stream::Stdout,
                                bytes,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }));
        let stderr_tx = chunk_tx.clone();
        handles.push(thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr_r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        if stderr_tx
                            .send(Chunk {
                                stream: Stream::Stderr,
                                bytes,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }));
        // Drop our only remaining sender handle so both threads' sends close
        // the channel when the subprocess exits and the pipes hit EOF.
        drop(chunk_tx);

        self.child = Some(child);
        self.stdin = Some(stdin);
        self.chunk_rx = Some(chunk_rx);
        self.reader_handles = handles;
        self.output_seq = 0;
        self.screen_seq = 0;
        self.content_seq = 0;
        self.bell_seq = 0;
        self.title_seq = 0;
        self.lines.clear();
        self.pending.clear();
        self.stdout_lines.clear();
        self.stderr_lines.clear();
        self.lines_evicted = 0;
        let now = Instant::now();
        self.last_output_instant = now;
        self.last_screen_change_instant = now;
        // Give the process a moment to emit initial output (parity).
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
        self.stdin = None;
        for handle in self.reader_handles.drain(..) {
            let _ = handle.join();
        }
        self.chunk_rx = None;
        self.child_pid = None;
        Ok(())
    }

    fn state(&mut self) -> BackendResult<ScreenState> {
        Ok(self.synth_screen())
    }

    fn send_input(&mut self, input: Input) -> BackendResult<()> {
        self.pump();
        if self.stdin.is_none() {
            return Err(BackendError::NoSession);
        }
        match input {
            Input::Text(t) | Input::Paste(t) => self.write_input(t.as_bytes())?,
            Input::Key(kev) => {
                // A pipe has no key semantics beyond bytes: a MODIFIED
                // keypress cannot be expressed (ctrl+c would silently
                // degrade to the letter 'c' — the exact dishonesty the
                // backend contract prohibits), so it is rejected.
                if kev.modifiers != crate::backend::KeyModifiers::NONE {
                    return Err(BackendError::Unsupported(format!(
                        "pipe backend cannot encode modified keys (got {kev:?}); use the portable engine for full key semantics"
                    )));
                }
                match kev.code {
                    crate::backend::KeyCode::Enter => self.write_input(b"\n")?,
                    crate::backend::KeyCode::Char(c) => {
                        let mut s = String::new();
                        s.push(c);
                        self.write_input(s.as_bytes())?;
                    }
                    _ => {
                        return Err(BackendError::Unsupported(
                            "pipe backend supports Char/Enter keys only; no terminal grid exists"
                                .into(),
                        ))
                    }
                }
            }
            Input::Keys(keys) => {
                for kev in keys {
                    if kev.modifiers != crate::backend::KeyModifiers::NONE {
                        return Err(BackendError::Unsupported(format!(
                            "pipe backend cannot encode modified keys (got {kev:?}); use the portable engine for full key semantics"
                        )));
                    }
                    match kev.code {
                        crate::backend::KeyCode::Enter => self.write_input(b"\n")?,
                        crate::backend::KeyCode::Char(c) => {
                            let mut s = String::new();
                            s.push(c);
                            self.write_input(s.as_bytes())?;
                        }
                        _ => {
                            return Err(BackendError::Unsupported(
                                "pipe backend supports Char/Enter keys only".into(),
                            ))
                        }
                    }
                }
            }
            Input::Raw(b) => self.write_input(&b)?,
            Input::Mouse(_) | Input::MouseClick { .. } => {
                return Err(BackendError::Unsupported(
                    "mouse is meaningless on a pipe backend (no terminal grid reports input)"
                        .into(),
                ))
            }
            Input::Resize { cols, rows } => {
                // Pipes have no size; the synthetic grid can be re-dimensioned
                // but the child is never told (there is no terminal to resize).
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
        self.cols = cols;
        self.rows = rows;
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref h) = *slot {
                h.on_resize(cols, rows);
            }
        }
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
            _ => self.bell_seq,
        };
        let baseline_interaction_seq = self.screen_seq + self.bell_seq;
        let baseline_screen_seq = self.screen_seq;
        loop {
            self.pump();
            let screen = self.synth_screen();
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
                WaitCond::ScreenChange => (
                    self.screen_seq > baseline_screen_seq,
                    WaitReason::ScreenChange,
                ),
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
                WaitCond::Bell { .. } => (self.bell_seq > baseline_bell_seq, WaitReason::Bell),
                WaitCond::AnyActivity {
                    after_interaction_seq,
                } => {
                    let cur = self.screen_seq + self.bell_seq;
                    let anchored_ok = match after_interaction_seq {
                        Some(seq) => cur > *seq,
                        None => cur > baseline_interaction_seq,
                    };
                    (anchored_ok, WaitReason::ScreenChange)
                }
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
                // OSC 133 shell integration is terminal-side; a pipe child
                // emitting it is possible but this backend does not parse it —
                // report unsatisfied honestly.
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

    /// Capture now, then settle on pipe-quiet (parity with line backend).
    fn observe(&mut self, idle: Duration) -> BackendResult<ObserveResult> {
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

    fn capabilities(&self) -> Capabilities {
        use super::{EventCapability, InputFamily, WaitCapability};
        Capabilities {
            mouse: false,
            kitty_keyboard: false,
            colors: false,
            cell_attributes: false,
            title: false,
            scrollback: true,
            bracketed_paste: false,
            signals: cfg!(unix),
            // The pipe engine does not retain a raw byte ring
            // (`recent_raw_output` stays on the trait default): nothing to
            // capture, so say false rather than overclaim.
            protocol_capture: false,
            // --- audit finding 37: operation-oriented matrix ---
            raw_input: true,          // Input::Raw writes bytes to the stdin pipe
            bell_observable: true,    // bell_seq tracked in wait()
            exit_code: true,          // process() reports the real child exit code
            shell_integration: false, // command_state() returns None (no OSC 133)
            // review P1 #28: the pipe backend IS the stdout/stderr split — the
            // child is launched with separate stdout/stderr pipes.
            stdout_stderr_separation: true,
            recording: true,       // recording hook delivered on output/input
            native_semantic: true, // session-provided side channel
            attach: false,         // we spawn the child
            process_ownership: super::ProcessOwnership::SpawnedChild,
            query_response: false, // no device-query responder
            event_types: vec![
                EventCapability::Output,
                EventCapability::Bell,
                EventCapability::FocusChanged,
                EventCapability::SemanticChanged,
            ],
            supported_waits: vec![
                WaitCapability::Text,
                WaitCapability::TextAbsent,
                WaitCapability::ScreenChange,
                WaitCapability::ScreenStable,
                WaitCapability::ProcessExit,
                WaitCapability::Bell,
                WaitCapability::AnyActivity,
                WaitCapability::Idle,
            ],
            input_families: vec![
                InputFamily::Key,
                InputFamily::Paste,
                InputFamily::RawByte,
                InputFamily::Resize,
                InputFamily::Signal,
            ],
        }
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

    fn set_clear_env(&mut self, clear: bool) {
        self.clear_env_on_start = clear;
    }

    fn recording(&self) -> bool {
        self.recorder_is_some()
    }

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
        // Viewport hits.
        for (y, row) in self.lines[start..].iter().enumerate() {
            let lower = row.to_lowercase();
            let mut from = 0;
            while let Some(rel) = lower[from..].find(&q) {
                let s = from + rel;
                hits.push(SearchHit {
                    region: "viewport".to_string(),
                    row: y as u32,
                    start: s as u32,
                    len: q.len() as u32,
                    line: row.clone(),
                });
                from = s + q.len();
            }
        }
        // Scrollback hits.
        for (y, row) in self.lines[..start].iter().enumerate() {
            let lower = row.to_lowercase();
            let mut from = 0;
            while let Some(rel) = lower[from..].find(&q) {
                let s = from + rel;
                hits.push(SearchHit {
                    region: "scrollback".to_string(),
                    row: y as u32,
                    start: s as u32,
                    len: q.len() as u32,
                    line: row.clone(),
                });
                from = s + q.len();
            }
        }
        Ok(hits)
    }

    fn command_state(&mut self) -> Option<CommandState> {
        // OSC 133 parsing is not implemented here: honest None.
        None
    }
}

/// Split a std `ExitStatus` into (code, signal-string). Signal extraction is
/// Unix-only; on other platforms we report the code with no signal, matching
/// the honest "we cannot tell" posture.
fn exit_status_parts(status: std::process::ExitStatus) -> (i32, Option<String>) {
    let code = status.code().unwrap_or(-1);
    #[cfg(unix)]
    let sig = std::os::unix::process::ExitStatusExt::signal(&status).map(|s| s.to_string());
    #[cfg(not(unix))]
    let sig = None;
    (code, sig)
}
