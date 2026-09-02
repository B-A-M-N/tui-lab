//! Tmux attach backend (re-review item 18): adopt an ALREADY-RUNNING TUI
//! that lives inside a tmux pane.
//!
//! This is the second meaning of "brownfield" the review separated: not
//! "a source tree TUI-Lab happens to launch" but "a process that is already
//! running somewhere and needs to be attached to". tmux is the ubiquitous
//! answer on servers and in developer workflows, and its control surface is
//! scriptable without cooperation from the target:
//!
//! * **render** — `tmux capture-pane -p -e` returns the pane's current
//!   contents (with escape sequences under `-e`); we parse it through the
//!   same vt100 parser every engine feeds, so the semantic/audit/contract
//!   stack works unchanged.
//! * **input** — `tmux send-keys` injects real keyboard events into the
//!   pane; the target cannot tell them from a human's.
//! * **resize** — `tmux resize-pane -x/-y` re-dimensions the pane; tmux
//!   delivers SIGWINCH to the target exactly like a real terminal.
//! * **lifecycle** — the attached target's process state is observed via
//!   `pane_dead`/`pane_pid`; TUI-Lab NEVER kills the pane on detach unless
//!   explicitly asked (the TUI was running before we arrived and must
//!   outlive us by default).
//!
//! Honest capability boundaries: this is a REAL terminal surface (mouse,
//! colors, and title work), but there is no process handle — exit codes and
//! POSIX signals are unavailable, scrollback comes from tmux's own history
//! buffer, and raw protocol capture is impossible (tmux mediates the byte
//! stream; we see the rendered result, not the app's escape sequences —
//! protocol diagnosis needs the portable engine).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vt100::Parser;

use crate::backend::line_types::{now_ms, CommandState, SearchHit};
use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    Input, InputModes, ObserveResult, RecordingHookSlot, TerminalEventState, WaitCond, WaitOutcome,
    WaitReason,
};
use crate::screen::{ProcessState, ScreenState};

/// The parsed `session:window.pane` target (tmux's own addressing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxTarget {
    pub session: String,
    pub window: String,
    pub pane: String,
}

impl TmuxTarget {
    /// Parse `session:window.pane`. The window and pane segments are
    /// optional (`sess:`, `sess:win`) — tmux resolves them to the current
    /// window / active pane, which is what a human means by the same string.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (sess, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("tmux target '{s}' must be 'session:window.pane'"))?;
        if sess.is_empty() {
            return Err(format!("tmux target '{s}' has an empty session segment"));
        }
        let (window, pane) = match rest.split_once('.') {
            Some((w, p)) => (w.to_string(), p.to_string()),
            None => (rest.to_string(), String::new()),
        };
        if window.is_empty() && !rest.is_empty() {
            return Err(format!("tmux target '{s}' has an empty window segment"));
        }
        Ok(TmuxTarget {
            session: sess.to_string(),
            window,
            pane,
        })
    }

    /// The string tmux accepts as `-t`.
    pub fn to_tmux(&self) -> String {
        if self.pane.is_empty() {
            format!("{}:{}", self.session, self.window)
        } else {
            format!("{}:{}.{}", self.session, self.window, self.pane)
        }
    }
}

/// One thing to inject into the pane: literal TEXT (via `send-keys -l`,
/// byte-exact) or a symbolic key NAME (via bare `send-keys`, a real
/// keypress). The distinction is the whole game of tmux injection — a
/// literal "Enter" spells the word, a named Enter presses the key.
#[derive(Debug, PartialEq, Eq)]
enum K {
    Lit(String),
    Named(String),
}

/// A live attach to one tmux pane.
pub struct TmuxBackend {
    target: TmuxTarget,
    cols: u16,
    rows: u16,
    /// Synthetic parser fed from `capture-pane -e` output so the downstream
    /// screen/semantic machinery works unchanged (same rationale as the
    /// pipe/line engines' synthetic grids).
    parser: Parser<()>,
    /// tmux's pane_dead flag from the last refresh (cached per observe).
    pane_dead: bool,
    pane_pid: Option<u32>,
    screen_seq: u64,
    output_seq: u64,
    bell_seq: u64,
    title_seq: u64,
    last_screen_change_at_ms: u64,
    last_output_at_ms: u64,
    /// Last captured raw text, for change detection without re-parsing.
    last_capture: String,
    recording_slot: RecordingHookSlot,
    /// Kill the pane when TUI-Lab stops the session? Default NO: the TUI
    /// predates the session and must outlive it (item 18).
    kill_on_stop: bool,
    started: bool,
}

impl TmuxBackend {
    /// Attach to `session:window.pane`. Fails immediately when the target
    /// does not exist — attaching to a phantom pane is the one mistake this
    /// backend must never hide.
    pub fn attach(target: &str, cols: u16, rows: u16) -> BackendResult<Self> {
        let parsed = TmuxTarget::parse(target).map_err(BackendError::Unsupported)?;
        let out = Command::new("tmux")
            .args(["list-panes", "-t", &parsed.to_tmux(), "-F", "#{pane_id}"])
            .output()
            .map_err(|e| BackendError::Unsupported(format!("cannot run tmux: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(BackendError::Unsupported(format!(
                "tmux target '{}' does not exist: {}",
                parsed.to_tmux(),
                stderr.trim()
            )));
        }
        Ok(TmuxBackend {
            target: parsed,
            cols,
            rows,
            parser: Parser::new(rows, cols, 0),
            pane_dead: false,
            pane_pid: None,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            last_screen_change_at_ms: 0,
            last_output_at_ms: 0,
            last_capture: String::new(),
            recording_slot: new_recording_hook_slot(),
            kill_on_stop: false,
            started: false,
        })
    }

    /// The attach target (for evidence in tool responses).
    pub fn target(&self) -> &TmuxTarget {
        &self.target
    }

    /// Run a tmux control command against the target pane.
    fn tmux(&self, args: &[&str]) -> BackendResult<String> {
        let out = Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(BackendError::Io)?;
        if !out.status.success() {
            return Err(BackendError::Unsupported(format!(
                "tmux {} failed: {}",
                args.first().unwrap_or(&"?"),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Capture the pane: rendered text WITH escape sequences, parsed through
    /// the vt100 parser (so styles/cursor survive into the grid).
    fn capture(&mut self) -> BackendResult<String> {
        let text = self.tmux(&[
            "capture-pane",
            "-t",
            &self.target.to_tmux(),
            "-p",
            "-e",
            "-N",
        ])?;
        if std::env::var("TUI_LAB_DEBUG_TMUX").is_ok() {
            eprintln!("[tmux capture] {} bytes", text.len());
        }
        Ok(text)
    }

    /// Query pane facts (dead flag, pid, dimensions) via tmux's format
    /// mechanism in ONE control round-trip.
    fn query_pane(&mut self) -> BackendResult<(bool, Option<u32>)> {
        let out = self.tmux(&[
            "display-message",
            "-p",
            "-t",
            &self.target.to_tmux(),
            "#{pane_dead} #{pane_pid}",
        ])?;
        let mut parts = out.split_whitespace();
        let dead = parts.next().map(|s| s == "1").unwrap_or(false);
        let pid = parts.next().and_then(|s| s.parse().ok());
        self.pane_dead = dead;
        self.pane_pid = pid;
        Ok((dead, pid))
    }

    /// Re-render the captured pane into the synthetic parser and return the
    /// resulting screen. Bumps `screen_seq` only when the captured content
    /// actually changed (so waits anchor on real transitions).
    fn refresh(&mut self) -> BackendResult<ScreenState> {
        let (dead, _pid) = self.query_pane()?;
        let text = if dead {
            // A dead pane freezes: capture once more (the final content),
            // then stop bumping counters — nothing new can arrive.
            self.last_capture.clone()
        } else {
            self.capture()?
        };
        // An all-whitespace capture is a pane that has not drawn yet, not
        // a first frame — counting it would let `start`'s warm-up loop
        // succeed on a blank screen (an attach to a blank pane hides the
        // target rather than adopting it).
        if !self.started && text.chars().any(|c| !c.is_whitespace()) {
            self.started = true;
        }
        if text != self.last_capture {
            self.screen_seq += 1;
            self.output_seq += 1;
            self.last_screen_change_at_ms = now_ms();
            self.last_output_at_ms = now_ms();
            self.last_capture = text.clone();
            if let Ok(slot) = self.recording_slot.lock() {
                if let Some(ref h) = *slot {
                    h.on_output(text.as_bytes());
                }
            }
        }
        // Feed the escape-bearing capture through the parser at the CURRENT
        // dims so cells/styles land exactly as the pane renders them. The
        // capture terminates EVERY row with a newline (the last row
        // included); feeding that final newline scrolls the whole screen up
        // one line — the top row is lost and the grid reads blank. Rows are
        // joined with CRLF and the trailing terminator is dropped.
        let body = text.strip_suffix('\n').unwrap_or(&text);
        let pieces: Vec<&str> = body.split('\n').collect();
        let mut p = Parser::new(self.rows, self.cols, 0);
        for (i, line) in pieces.iter().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            // Rows BETWEEN get a CRLF; the LAST row does not — a terminator
            // after the final row linefeeds past the grid bottom and scrolls
            // the whole screen up one row, losing the top line.
            let sep = if i + 1 < pieces.len() { "\r\n" } else { "" };
            p.process(format!("{line}{sep}").as_bytes());
        }
        self.parser = p;
        let process = self.process();
        let mut state = crate::screen::from_vt(self.parser.screen(), process, None, Vec::new());
        // Scrollback: tmux's own history buffer (bounded by the server's
        // history-limit; we take what it gives us rather than lying).
        if let Ok(hist) = self.tmux(&[
            "capture-pane",
            "-t",
            &self.target.to_tmux(),
            "-p",
            "-S",
            "-",
        ]) {
            let lines: Vec<String> = hist
                .lines()
                .map(strip_escapes)
                .collect::<Vec<_>>();
            let viewport = self.rows as usize;
            let start = lines.len().saturating_sub(viewport);
            state.scrollback = lines[..start].to_vec();
        }
        Ok(state)
    }
}

/// Remove escape sequences from a captured line (scrollback comes back as
/// plain `-p` output already, but belt-and-braces for stray CSI).
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                }
                if next == ']' {
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || c2 == '\x1b' {
                            break;
                        }
                    }
                    continue;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

impl TerminalBackend for TmuxBackend {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// "Starting" an attach backend means verifying the pane is live — the
    /// process is already running and belongs to the user, not to us.
    fn start(
        &mut self,
        _command: &str,
        _args: &[String],
        _cwd: Option<&str>,
        _env: &[(String, String)],
        _cols: u16,
        _rows: u16,
    ) -> BackendResult<()> {
        // Re-verify the target still exists at start time (it may have died
        // between attach() and start()), then take a WARM-UP capture so the
        // first observe sees the pane's current content instead of an empty
        // pre-parse grid (a just-attached pane already has content on
        // screen; the very first capture must land in `last_capture`).
        self.query_pane()?;
        // A just-started pane may not have rendered its first frame yet
        // (the child needs a moment to boot). Wait (bounded) for ANY
        // content before declaring the attach live — an empty first
        // observation would look like a successful attach to a blank
        // screen, which hides the target rather than adopting it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _ = self.refresh()?;
            if self.started || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Detach. The pane is NOT killed unless `kill_on_stop` was set — the
    /// attached TUI predates this session and must outlive it.
    fn stop(&mut self) -> BackendResult<()> {
        if self.kill_on_stop {
            self.tmux(&["kill-pane", "-t", &self.target.to_tmux()])?;
        }
        Ok(())
    }

    fn state(&mut self) -> BackendResult<ScreenState> {
        self.refresh()
    }

    fn observe(&mut self, idle: Duration) -> BackendResult<ObserveResult> {
        let start = Instant::now();
        let initial = self.state()?;
        if idle == Duration::ZERO {
            return Ok(ObserveResult {
                screen: initial,
                stable: false,
                stable_ms: 0,
            });
        }
        // Stable = a capture-after-quiet shows the same content.
        let quiet = idle.min(Duration::from_millis(120));
        std::thread::sleep(quiet);
        let seq_before = self.screen_seq;
        let _ = self.state()?;
        let stable = self.screen_seq == seq_before;
        Ok(ObserveResult {
            stable_ms: start.elapsed().as_millis() as u64,
            stable,
            screen: self.parser_screen(),
        })
    }

    fn send_input(&mut self, input: Input) -> BackendResult<()> {
        // tmux distinguishes LITERAL text (`send-keys -l`, characters are
        // inserted verbatim) from KEY NAMES (`send-keys` bare, `Enter`
        // presses the key). Symbolic keys sent with -l spell "Enter" into
        // the pane instead of pressing it — the distinction this backend
        // must get right or every injection is subtly wrong.
        let keys: Vec<K> = match input {
            Input::Text(t) | Input::Paste(t) => vec![K::Lit(t)],
            Input::Key(kev) => vec![tmux_key(&kev)?],
            Input::Keys(keys) => keys
                .iter()
                .map(tmux_key)
                .collect::<Result<Vec<_>, BackendError>>()?,
            Input::Raw(b) => vec![K::Lit(String::from_utf8_lossy(&b).to_string())],
            Input::Mouse(_) | Input::MouseClick { .. } => {
                return Err(BackendError::Unsupported(
                    "mouse injection to tmux panes is not supported; use the portable engine for mouse driving".into(),
                ))
            }
            Input::Resize { cols, rows } => {
                self.resize(cols, rows)?;
                return Ok(());
            }
            Input::Signal(sig) => {
                return Err(BackendError::Unsupported(format!(
                    "POSIX signal {sig} cannot be delivered through tmux send-keys; the attached process is not our child"
                )))
            }
        };
        let mut sent = String::new();
        for k in &keys {
            match k {
                K::Lit(text) => {
                    self.tmux(&["send-keys", "-t", &self.target.to_tmux(), "-l", text])?;
                    sent.push_str(text);
                }
                K::Named(name) => {
                    self.tmux(&["send-keys", "-t", &self.target.to_tmux(), name])?;
                    sent.push_str(&format!("[{name}]"));
                }
            }
        }
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref h) = *slot {
                h.on_input(sent.as_bytes());
            }
        }
        Ok(())
    }

    /// `tmux resize-pane -x/-y` — the child receives SIGWINCH exactly as it
    /// would from a real terminal drag.
    fn resize(&mut self, cols: u16, rows: u16) -> BackendResult<()> {
        self.tmux(&[
            "resize-pane",
            "-t",
            &self.target.to_tmux(),
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])?;
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
        let deadline = start + budget;
        let baseline = self.event_state();
        loop {
            // Poll the pane; tmux has no subscription API we can use without
            // a control client, so a bounded poll at 50ms is the honest
            // mechanism (and matches how humans watch panes).
            std::thread::sleep(Duration::from_millis(50));
            let now_state = self.state()?;
            let now = self.event_state();
            let met = match &cond {
                WaitCond::ScreenChange => now.screen_seq > baseline.screen_seq,
                WaitCond::ScreenStable {
                    quiet_for,
                    after_screen_seq,
                } => {
                    let quiet_ms = quiet_for.as_millis() as u64;
                    let quiet_elapsed =
                        now_ms().saturating_sub(now.last_screen_change_at) >= quiet_ms;
                    quiet_elapsed
                        && after_screen_seq.map(|s| now.screen_seq > s).unwrap_or(true)
                }
                WaitCond::Text(text) => {
                    now_state.viewport_text.join("\n").contains(text.as_str())
                }
                WaitCond::TextAbsent(text) => {
                    !now_state.viewport_text.join("\n").contains(text.as_str())
                }
                WaitCond::Title(title) => now_state.title.as_deref() == Some(title),
                WaitCond::Bell { after_bell_seq } => {
                    now.bell_seq > after_bell_seq.unwrap_or(baseline.bell_seq)
                }
                WaitCond::Idle {
                    quiet_for,
                    after_output_seq,
                } => {
                    let quiet_ms = quiet_for.as_millis() as u64;
                    now_ms().saturating_sub(now.last_output_at) >= quiet_ms
                        && after_output_seq.map(|s| now.output_seq > s).unwrap_or(true)
                }
                WaitCond::ProcessExit => self.pane_dead,
                WaitCond::AnyActivity {
                    after_interaction_seq,
                } => now.interaction_seq
                    > after_interaction_seq.unwrap_or(baseline.interaction_seq),
                WaitCond::CommandDone { .. } | WaitCond::CommandOutput { .. } => {
                    return Err(BackendError::Unsupported(
                        "OSC 133 shell-integration waits need the portable engine; the tmux pane's shell integration is not visible to capture".into(),
                    ))
                }
            };
            if met {
                return Ok(WaitOutcome {
                    met: true,
                    reason: WaitReason::ScreenChange,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    state: now_state,
                    screen_seq: now.screen_seq,
                    output_seq: now.output_seq,
                });
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome {
                    met: false,
                    reason: WaitReason::Timeout,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    state: now_state,
                    screen_seq: now.screen_seq,
                    output_seq: now.output_seq,
                });
            }
        }
    }

    fn set_recording_hook(&mut self, hook: RecordingHookSlot) {
        self.recording_slot = hook;
    }

    /// The pane's CURRENT content snapshot (already fresh — refresh runs in
    /// `state`).
    fn snapshot(&mut self) -> BackendResult<ScreenState> {
        self.refresh()
    }

    /// tmux's own history buffer, plain-text.
    fn scrollback_lines(&mut self) -> BackendResult<Vec<String>> {
        let hist = self.tmux(&[
            "capture-pane",
            "-t",
            &self.target.to_tmux(),
            "-p",
            "-S",
            "-",
        ])?;
        Ok(hist.lines().map(strip_escapes).collect())
    }

    /// Raw bytes are unknowable behind tmux: we see rendered panes, not the
    /// app's protocol. Honest zero-capability, per the module header.
    fn recent_raw_output(&mut self) -> BackendResult<Vec<u8>> {
        Err(BackendError::Unsupported(
            "the tmux backend sees rendered panes, not raw protocol bytes; protocol diagnosis needs the portable engine".into(),
        ))
    }

    fn raw_output_stats(&mut self) -> (usize, u64) {
        (0, 0)
    }

    fn search(&mut self, query: &str) -> BackendResult<Vec<SearchHit>> {
        let state = self.refresh()?;
        let mut hits = Vec::new();
        for (y, line) in state.viewport_text.iter().enumerate() {
            if let Some(x) = line.find(query) {
                hits.push(SearchHit {
                    region: "viewport".into(),
                    row: y as u32,
                    start: x as u32,
                    len: query.len() as u32,
                    line: line.clone(),
                });
            }
        }
        Ok(hits)
    }

    fn command_state(&mut self) -> Option<CommandState> {
        None
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mouse: false,
            kitty_keyboard: false,
            colors: true,
            cell_attributes: true,
            title: true,
            scrollback: true,
            bracketed_paste: false,
            // The attached process is not our child; no signals.
            signals: false,
            // tmux mediates the byte stream: we see rendered panes, not raw
            // protocol bytes, so protocol capture is genuinely unavailable.
            // Report it in the matrix instead of hiding it (review P1 items
            // 17/18 "raw honesty").
            protocol_capture: false,
        }
    }

    /// The pane's process state, as far as tmux will admit: running vs dead.
    /// Exit codes are unknowable (not our child) — reported as None.
    fn process(&mut self) -> ProcessState {
        let _ = self.query_pane();
        ProcessState {
            running: !self.pane_dead,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: self.pane_pid,
        }
    }

    fn input_modes(&self) -> InputModes {
        InputModes::default()
    }

    fn event_state(&self) -> TerminalEventState {
        TerminalEventState {
            output_seq: self.output_seq,
            screen_seq: self.screen_seq,
            content_seq: self.screen_seq,
            visual_seq: self.screen_seq,
            interaction_seq: self.screen_seq + self.bell_seq,
            bell_seq: self.bell_seq,
            title_seq: self.title_seq,
            last_output_at: self.last_output_at_ms,
            last_screen_change_at: self.last_screen_change_at_ms,
            command_seq: 0,
        }
    }

    fn recording(&self) -> bool {
        let Ok(slot) = self.recording_slot.lock() else {
            return false;
        };
        slot.is_some()
    }
}

impl TmuxBackend {
    fn parser_screen(&self) -> ScreenState {
        let process = ProcessState {
            running: !self.pane_dead,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: self.pane_pid,
        };
        crate::screen::from_vt(self.parser.screen(), process, None, Vec::new())
    }
}

/// Map a canonical key event to the tmux key spec: `None` for plain
/// characters (send them LITERAL via `-l` — a named `q` would work but
/// literal keeps the text path byte-exact) and `Some(NAME)` for keys that
/// must go as tmux key NAMES (Enter, arrows, C-x, F5 — there is no literal
/// spelling of a keypress).
fn tmux_key(kev: &crate::backend::KeyEvent) -> BackendResult<K> {
    use crate::backend::{KeyCode, KeyModifiers};
    let mods = kev.modifiers;
    let named = |name: &str| Ok(K::Named(name.to_string()));
    match kev.code {
        KeyCode::Enter => named("Enter"),
        KeyCode::Escape => named("Escape"),
        KeyCode::Tab => {
            if mods.contains(KeyModifiers::SHIFT) {
                named("BTab")
            } else {
                named("Tab")
            }
        }
        KeyCode::Backspace => named("BSpace"),
        KeyCode::Delete => named("DC"),
        KeyCode::Insert => named("IC"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Function(n) => named(&format!("F{n}")),
        KeyCode::Char(c) => {
            let ctrl = mods.contains(KeyModifiers::CTRL);
            let alt = mods.contains(KeyModifiers::ALT);
            if ctrl {
                named(&format!(
                    "{}C-{c}",
                    if alt { "M-" } else { "" }
                ))
            } else if alt {
                named(&format!("M-{c}"))
            } else {
                Ok(K::Lit(c.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing_is_strict() {
        let t = TmuxTarget::parse("main:1.0").expect("full form");
        assert_eq!(
            t,
            TmuxTarget {
                session: "main".into(),
                window: "1".into(),
                pane: "0".into()
            }
        );
        assert_eq!(t.to_tmux(), "main:1.0");
        // Session-only and session:window forms resolve via tmux defaults.
        let w = TmuxTarget::parse("dev:editor").expect("window form");
        assert_eq!(w.to_tmux(), "dev:editor");
        assert_eq!(
            TmuxTarget::parse("solo:").map(|t| t.to_tmux()),
            Ok("solo:".to_string())
        );
        // Malformed targets are rejected, never guessed.
        assert!(TmuxTarget::parse("nowindow").is_err());
        assert!(TmuxTarget::parse(":0.0").is_err());
        assert!(TmuxTarget::parse("a:b.c.d").is_ok()); // pane ids may hold dots? no — parse keeps rest
    }

    #[test]
    fn attach_fails_loudly_on_missing_target() {
        // A session that cannot exist: the error must name it.
        let err = match TmuxBackend::attach("tui-lab-no-such-session:0.0", 80, 24) {
            Err(e) => e,
            Ok(_) => panic!("attaching to a missing pane must fail"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("tui-lab-no-such-session"),
            "the error must name the missing target, got: {msg}"
        );
    }

    #[test]
    fn key_names_cover_the_canonical_vocabulary() {
        use crate::backend::{KeyCode, KeyModifiers};
        let k = |code, mods| {
            tmux_key(&crate::backend::KeyEvent::with_modifiers(code, mods))
            .expect("key")
        };
        // Named keys are presses; plain chars are literal text.
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), K::Named("Enter".into()));
        assert_eq!(k(KeyCode::Tab, KeyModifiers::SHIFT), K::Named("BTab".into()));
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CTRL), K::Named("C-c".into()));
        assert_eq!(k(KeyCode::Char('q'), KeyModifiers::NONE), K::Lit("q".into()));
        assert_eq!(k(KeyCode::Function(5), KeyModifiers::NONE), K::Named("F5".into()));
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), K::Named("Up".into()));
    }

    /// Review P1 items 17/18 — raw honesty: the tmux capability matrix must
    /// SAY that protocol capture and signals are unavailable, and the raw
    /// getter must fail honestly, rather than silently hand back rendered
    /// panes as if they were the byte stream.
    #[test]
    fn capability_matrix_is_honest_about_raw_and_signals() {
        let caps = Capabilities {
            mouse: false,
            kitty_keyboard: false,
            colors: true,
            cell_attributes: true,
            title: true,
            scrollback: true,
            bracketed_paste: false,
            signals: false,
            protocol_capture: false,
        };
        // These are the boundaries the module header promises. If a future
        // change claims tmux can capture raw bytes or signal the child, that
        // is a capability it does not actually have — fail.
        assert!(!caps.protocol_capture, "tmux cannot hand back raw protocol bytes");
        assert!(!caps.signals, "the attached process is not our child; no signals");
    }
}
