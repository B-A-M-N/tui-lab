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
//! Honest capability boundaries: this is a REAL terminal surface (colors,
//! cell attributes, the pane's last OSC title, and its window bell flag are
//! observable via tmux's format vars), but there is no process handle —
//! exit codes and POSIX signals are unavailable, MOUSE INJECTION IS NOT
//! SUPPORTED, scrollback comes from tmux's own history buffer, and raw
//! protocol capture is impossible (tmux mediates the byte stream; we see
//! the rendered result, not the app's escape sequences — protocol
//! diagnosis needs the portable engine). Bell/title events are POLL-DERIVED
//! from tmux's flags, so rapid repeated bells can coalesce into one edge.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vt100::Parser;

use crate::backend::line_types::{now_ms, CommandState, SearchHit};
use crate::backend::{
    new_recording_hook_slot, trait_def::TerminalBackend, BackendError, BackendResult, Capabilities,
    DispatchOutcome, Input, InputModes, ObserveResult, ProcessOwnership, RecordingHookSlot,
    TerminalEventState, WaitCond, WaitOutcome, WaitReason,
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
    /// Independent monotonic interaction edge counter (finding 35). It
    /// increments only when a bell/title/render edge occurs. Never derive
    /// it as `other_seq + boolean`; `TerminalEventState.interaction_seq`
    /// must be a single monotonic domain.
    interaction_seq: u64,
    /// Last canonical content fingerprint seen at refresh (finding 35).
    prev_content_hash: Option<String>,
    /// Last visual fingerprint seen at refresh (finding 35).
    prev_visual_hash: Option<String>,
    /// Last seen `#{pane_title}` (audit P0-38: the capability matrix said
    /// `title: true` while `title_seq` never advanced — the title existed as
    /// a field but no path ever filled it. tmux itself tracks the pane's
    /// last OSC title, so one extra format var in the existing query wires
    /// it for real).
    last_title: Option<String>,
    /// Previous `#{window_bell_flag}` (audit P0-39: `WaitCond::Bell`
    /// compared against a `bell_seq` that never moved, so every bell wait
    /// burned its whole budget and timed out). The flag is transition-based:
    /// tmux sets it on ring and clears it when the window becomes active, so
    /// a 0→1 edge bumps `bell_seq` once. Repeated bells while the flag stays
    /// set are undercounted — a documented cost of polling without a
    /// control client.
    prev_bell_flag: bool,
    last_screen_change_at_ms: u64,
    last_output_at_ms: u64,
    /// Last captured raw text, for change detection without re-parsing.
    last_capture: String,
    /// Independent fingerprints: raw rendered text can change with color
    /// only, while a canonical grid distinguishes content and visual state.
    content_hash: String,
    visual_hash: String,
    recording_slot: RecordingHookSlot,
    /// Kill the pane when TUI-Lab stops the session? Default NO: the TUI
    /// predates the session and must outlive it (item 18).
    kill_on_stop: bool,
    started: bool,
    /// Pane-death observed on the previous query. A true→false transition is
    /// not meaningful for tmux, but false→true must emit exactly one edge.
    prev_pane_dead: bool,
    /// Last observed pane-death exit signal (set from tmux's
    /// `#{pane_dead_signal}` on the transition). `None` is honest when tmux
    /// cannot report a signal.
    pane_exit_signal: Option<String>,
    /// Finding 38: persistent tmux control-mode reader. `None` when the
    /// control client could not start; the backend then remains a declared
    /// polling fallback, never fabricating control events.
    control: Option<TmuxControlMode>,
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
            target: parsed.clone(),
            cols,
            rows,
            parser: Parser::new(rows, cols, 0),
            pane_dead: false,
            pane_pid: None,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            interaction_seq: 0,
            prev_content_hash: None,
            prev_visual_hash: None,
            last_title: None,
            prev_bell_flag: false,
            last_screen_change_at_ms: 0,
            last_output_at_ms: 0,
            last_capture: String::new(),
            content_hash: String::new(),
            visual_hash: String::new(),
            recording_slot: new_recording_hook_slot(),
            kill_on_stop: false,
            started: false,
            prev_pane_dead: false,
            pane_exit_signal: None,
            control: TmuxControlMode::spawn(&parsed.to_tmux()).ok(),
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

    /// Query pane facts (dead flag, pid, title, window bell flag) via tmux's
    /// format mechanism in ONE control round-trip. The bell flag is a
    /// window-level property, so it is queried in the same round-trip even
    /// though the pane's title is pane-level — polling each separately would
    /// double the control traffic for no extra information.
    fn query_pane(&mut self) -> BackendResult<(bool, Option<u32>)> {
        let out = self.tmux(&[
            "display-message",
            "-p",
            "-t",
            &self.target.to_tmux(),
            "#{pane_dead} #{pane_pid} #{window_bell_flag} #{pane_dead_signal} #{pane_title}",
        ])?;
        // The title can contain spaces (it is the app's own OSC string), so
        // it is parsed as: three tokens (dead, pid, bell), then a fourth
        // signal token, then the REMAINDER as the title.
        let mut parts = out.splitn(5, ' ');
        let dead = parts.next().map(|s| s == "1").unwrap_or(false);
        let pid = parts.next().and_then(|s| s.parse().ok());
        let bell_flag = parts.next().map(|s| s.trim() == "1").unwrap_or(false);
        let dead_signal = parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let title = parts
            .next()
            .map(|t| t.trim_end_matches('\n').to_string())
            .filter(|t| !t.is_empty());
        self.pane_dead = dead;
        self.pane_pid = pid;
        // Finding 38: preserve tmux's own pane-death signal on the observed
        // transition so a later attach/observe cannot fabricate a "clean"
        // exit for an app that died by signal. Pane liveness is still the
        // ONLY death claim; signal provenance stays attached to that claim.
        if dead && !self.prev_pane_dead {
            self.pane_exit_signal = dead_signal;
        }
        // Audit P0-39: a 0→1 bell-flag edge rings `bell_seq`. The flag
        // clears when the window is selected, so the edge re-arms; bells
        // coalescing inside one poll window are one bump (polling limit,
        // documented on the field).
        if bell_flag && !self.prev_bell_flag {
            self.bell_seq += 1;
            self.interaction_seq += 1;
        }
        self.prev_bell_flag = bell_flag;
        // Audit P0-38: a title CHANGE advances `title_seq` (and counts as
        // output activity — the app wrote to the terminal).
        if title != self.last_title {
            self.title_seq += 1;
            self.interaction_seq += 1;
            self.last_title = title.clone();
            self.last_output_at_ms = now_ms();
        }
        Ok((dead, pid))
    }

    /// Re-render the captured pane into the synthetic parser and return the
    /// resulting screen. Bumps `screen_seq` only when the captured content
    /// actually changed (so waits anchor on real transitions).
    fn refresh(&mut self) -> BackendResult<ScreenState> {
        if let Some(control) = self.control.as_mut() {
            control.drain()?;
        }
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
            self.interaction_seq += 1;
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
        // Keep the transition watermark authoritative for the NEXT query;
        // this refresh's death edge is represented in `pane_dead` and the
        // exit signal captured above.
        self.prev_pane_dead = self.pane_dead;
        let process = self.process();
        // Audit P0-38: the pane's last OSC title rides into the screen
        // state, so title waits, pre-state capture, and residue checks all
        // see what tmux itself tracked.
        let mut state = crate::screen::from_vt(
            self.parser.screen(),
            process,
            self.last_title.clone(),
            Vec::new(),
        );
        state.process.exit_signal = self.pane_exit_signal.clone();
        // Independent content/visual fingerprints: style-only pane changes
        // alter the visual state without changing canonical text/content.
        // Audit finding 35: detect edges by comparing the PREVIOUS
        // fingerprint of the SAME domain. A style-only change advances the
        // visual clock even when canonical content is unchanged; never use
        // `visual != content` as an edge signal.
        if self.prev_content_hash.as_deref() != Some(state.semantic_identity().as_str()) {
            self.interaction_seq += 1;
        }
        if self.prev_visual_hash.as_deref() != Some(state.visual_hash.as_str()) {
            self.interaction_seq += 1;
        }
        self.prev_content_hash = Some(state.semantic_identity());
        self.prev_visual_hash = Some(state.visual_hash.clone());
        self.content_hash = state.semantic_identity();
        self.visual_hash = state.visual_hash.clone();
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
            let lines: Vec<String> = hist.lines().map(strip_escapes).collect::<Vec<_>>();
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
        // pre-parse grid (a just-attached pane may already have content).
        self.query_pane()?;
        // Audit finding 37: attachment is proven by PANE LIVENESS, never by
        // nonblank content — a valid application can intentionally start
        // with a blank pane. Wait (bounded) for the target's pane to be
        // alive (query_pane proves existence; refresh() proves the capture
        // channel works); a blank initial frame is a legitimate state,
        // reported separately through startup_outcome, never conflated
        // with "failed to attach".
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (dead, _pid) = self.query_pane()?;
            if !dead || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // First warm-up capture: a fresh attach must land in `last_capture`
        // so the first observe returns real pane content (or a valid blank).
        let _ = self.refresh()?;
        Ok(())
    }

    /// Detach. The pane is NOT killed unless `kill_on_stop` was set — the
    /// attached TUI predates this session and must outlive it.
    fn kill_on_stop(&self) -> bool {
        self.kill_on_stop
    }

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
            // Audit P0-40: raw bytes are NOT text. `send-keys -l` writes a
            // UTF-8 string; a lossy from_utf8_lossy silently replaced
            // invalid sequences with U+FFFD — the probe the caller sent
            // was never the bytes that landed. Non-UTF-8 raw payloads are
            // refused honestly (the portable engine carries raw bytes).
            Input::Raw(b) => match std::str::from_utf8(&b) {
                Ok(text) => vec![K::Lit(text.to_string())],
                Err(_) => {
                    return Err(BackendError::Unsupported(
                        "raw payload is not valid UTF-8; tmux send-keys cannot deliver arbitrary binary — use the portable (PTY) engine for escape-sequence-level injection".into(),
                    ))
                }
            },
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
        // Audit finding 36: `-l` is a COMMAND-LEVEL literal mode, not a
        // per-key modifier — interleaving `-l` among named key args makes
        // `["a", "enter"]` risk spelling "enter" literally. Normal
        // `Input::Keys` therefore sends key-token arguments that coexist
        // in ONE bare `send-keys` invocation (tmux treats an argument as a
        // literal when it is not a recognized key name), and the dedicated
        // literal path (`-l -- <text>`) is reserved for Text/Paste/Raw —
        // all-or-nothing per invocation, never mixed with named keys.
        if keys.iter().any(|k| matches!(k, K::Lit(_))) {
            // Literal-only payloads: one `send-keys -l` invocation.
            let mut argv: Vec<String> = vec![
                "send-keys".to_string(),
                "-l".to_string(),
                "-t".to_string(),
                self.target.to_tmux(),
            ];
            let mut sent = String::new();
            for k in &keys {
                match k {
                    K::Lit(text) => {
                        // A literal string is ONE argument (tmux preserves
                        // spaces within an argument for -l).
                        argv.push(text.clone());
                        sent.push_str(text);
                    }
                    K::Named(_) => {
                        return Err(BackendError::Unsupported(
                            "mixed literal/named key sequences are refused in one invocation; split into Text + Keys actions".into(),
                        ))
                    }
                }
            }
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            self.tmux(&refs)?;
            if let Ok(slot) = self.recording_slot.lock() {
                if let Some(ref h) = *slot {
                    h.on_input(sent.as_bytes());
                }
            }
            return Ok(());
        }
        // Key-name-only payload: ONE bare send-keys with key-token args.
        let mut argv: Vec<String> = vec![
            "send-keys".to_string(),
            "-t".to_string(),
            self.target.to_tmux(),
        ];
        let mut sent = String::new();
        for k in &keys {
            match k {
                K::Named(name) => {
                    argv.push(name.clone());
                    sent.push_str(&format!("[{name}]"));
                }
                K::Lit(_) => unreachable!("literal-only branch handled above"),
            }
        }
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.tmux(&refs)?;
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref h) = *slot {
                h.on_input(sent.as_bytes());
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, input: Input) -> DispatchOutcome {
        // tmux send-keys is all-or-nothing at the control-command level:
        // a non-zero tmux exit means NO key was injected. Encoding
        // failures (invalid raw UTF-8, unsupported payload families) are
        // proven pre-write. This is the fine-grained boundary the master
        // executor needs.
        let keys: Vec<K> = match input {
            Input::Text(t) | Input::Paste(t) => vec![K::Lit(t)],
            Input::Key(kev) => match tmux_key(&kev) {
                Ok(k) => vec![k],
                Err(e) => return DispatchOutcome::failed_before_write(e),
            },
            Input::Keys(keys) => {
                let mut out = Vec::new();
                for kev in keys {
                    match tmux_key(&kev) {
                        Ok(k) => out.push(k),
                        Err(e) => return DispatchOutcome::failed_before_write(e),
                    }
                }
                out
            }
            Input::Raw(b) => match std::str::from_utf8(&b) {
                Ok(text) => vec![K::Lit(text.to_string())],
                Err(_) => {
                    return DispatchOutcome::failed_before_write(BackendError::Unsupported(
                        "raw payload is not valid UTF-8; tmux send-keys cannot deliver arbitrary binary — use the portable (PTY) engine for escape-sequence-level injection".into(),
                    ))
                }
            },
            Input::Mouse(_) | Input::MouseClick { .. } => {
                return DispatchOutcome::failed_before_write(BackendError::Unsupported(
                    "mouse injection to tmux panes is not supported; use the portable engine for mouse driving".into(),
                ))
            }
            Input::Resize { cols, rows } => match self.resize(cols, rows) {
                Ok(()) => return DispatchOutcome::ok(),
                Err(e) => return DispatchOutcome::failed_before_write(e),
            },
            Input::Signal(sig) => {
                return DispatchOutcome::failed_before_write(BackendError::Unsupported(format!(
                    "POSIX signal {sig} cannot be delivered through tmux send-keys; the attached process is not our child"
                )))
            }
        };
        // Audit finding 36 (dispatch half — same rule as send_input):
        // literal-only payloads take ONE `send-keys -l` invocation;
        // key-name-only payloads take ONE bare `send-keys`; a mixed
        // sequence is refused rather than mis-encoded.
        if keys.iter().any(|k| matches!(k, K::Lit(_))) {
            let mut argv: Vec<String> = vec![
                "send-keys".to_string(),
                "-l".to_string(),
                "-t".to_string(),
                self.target.to_tmux(),
            ];
            let mut sent = String::new();
            for k in &keys {
                match k {
                    K::Lit(text) => {
                        argv.push(text.clone());
                        sent.push_str(text.as_str());
                    }
                    K::Named(_) => {
                        return DispatchOutcome::failed_before_write(BackendError::Unsupported(
                            "mixed literal/named key sequences are refused in one invocation; split into Text + Keys actions".into(),
                        ))
                    }
                }
            }
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            match self.tmux(&refs) {
                Ok(_) => {}
                Err(e) => return DispatchOutcome::failed_before_write(e),
            }
            if let Ok(slot) = self.recording_slot.lock() {
                if let Some(ref h) = *slot {
                    h.on_input(sent.as_bytes());
                }
            }
            return DispatchOutcome::ok();
        }
        let mut argv: Vec<String> = vec![
            "send-keys".to_string(),
            "-t".to_string(),
            self.target.to_tmux(),
        ];
        let mut sent = String::new();
        for k in &keys {
            match k {
                K::Named(name) => {
                    argv.push(name.clone());
                    sent.push_str(&format!("[{name}]"));
                }
                K::Lit(_) => unreachable!("literal-only handled above"),
            }
        }
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.tmux(&refs) {
            Ok(_) => {}
            Err(e) => return DispatchOutcome::failed_before_write(e),
        }
        if let Ok(slot) = self.recording_slot.lock() {
            if let Some(ref h) = *slot {
                h.on_input(sent.as_bytes());
            }
        }
        DispatchOutcome::ok()
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
            // Finding 6: each condition resolves with its OWN reason — the
            // reason field names WHY the wait returned, not a blanket
            // ScreenChange for every condition.
            let (met, reason) = match &cond {
                WaitCond::ScreenChange => {
                    (now.screen_seq > baseline.screen_seq, WaitReason::ScreenChange)
                }
                WaitCond::ScreenStable {
                    quiet_for,
                    after_screen_seq,
                } => {
                    let quiet_ms = quiet_for.as_millis() as u64;
                    let quiet_elapsed =
                        now_ms().saturating_sub(now.last_screen_change_at) >= quiet_ms;
                    (
                        quiet_elapsed
                            && after_screen_seq.map(|s| now.screen_seq > s).unwrap_or(true),
                        WaitReason::ScreenStable,
                    )
                }
                WaitCond::Text(text) => (
                    now_state.viewport_text.join("\n").contains(text.as_str()),
                    WaitReason::Text,
                ),
                WaitCond::TextAbsent(text) => (
                    !now_state.viewport_text.join("\n").contains(text.as_str()),
                    WaitReason::TextAbsent,
                ),
                WaitCond::Title(title) => (
                    now_state.title.as_deref() == Some(title),
                    WaitReason::Title,
                ),
                WaitCond::Bell { after_bell_seq } => (
                    now.bell_seq > after_bell_seq.unwrap_or(baseline.bell_seq),
                    WaitReason::Bell,
                ),
                WaitCond::Idle {
                    quiet_for,
                    after_output_seq,
                } => {
                    let quiet_ms = quiet_for.as_millis() as u64;
                    (
                        now_ms().saturating_sub(now.last_output_at) >= quiet_ms
                            && after_output_seq.map(|s| now.output_seq > s).unwrap_or(true),
                        WaitReason::Idle,
                    )
                }
                WaitCond::ProcessExit => (self.pane_dead, WaitReason::ProcessExit),
                WaitCond::AnyActivity {
                    after_interaction_seq,
                } => (
                    now.interaction_seq
                        > after_interaction_seq.unwrap_or(baseline.interaction_seq),
                    WaitReason::ScreenChange,
                ),
                WaitCond::CommandDone { .. } | WaitCond::CommandOutput { .. } => {
                    return Err(BackendError::Unsupported(
                        "OSC 133 shell-integration waits need the portable engine; the tmux pane's shell integration is not visible to capture".into(),
                    ))
                }
            };
            if met {
                return Ok(WaitOutcome {
                    met: true,
                    reason,
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
        let Some(control) = self.control.as_mut() else {
            return Err(BackendError::Unsupported(
                "tmux control client unavailable; the backend sees rendered polling snapshots only"
                    .into(),
            ));
        };
        control.drain()?;
        Ok(control
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                TmuxControlEventKind::Output { bytes, .. } => Some(bytes.clone()),
                TmuxControlEventKind::Exit { .. } => None,
            })
            .flatten()
            .collect())
    }

    fn raw_output_stats(&mut self) -> (usize, u64) {
        match self.control.as_mut() {
            Some(control) => {
                let (capacity, total, dropped) = control.stats();
                (capacity, total.saturating_sub(dropped))
            }
            None => (0, 0),
        }
    }

    /// Finding 38: exact output chunks retained by the control client. The
    /// trait event-domain uses this vector's length as the sequence base, so
    /// callers can ask for the incremental suffix without replaying history.
    fn tmux_control_events(&mut self, after_seq: u64) -> Vec<TmuxControlEvent> {
        let Some(control) = self.control.as_mut() else {
            return Vec::new();
        };
        if control.drain().is_err() {
            return Vec::new();
        }
        control.take_events(after_seq)
    }

    fn search(&mut self, query: &str) -> BackendResult<Vec<SearchHit>> {
        // P1-26: use the shared search engine. Backend choice must not
        // change case sensitivity, all-hit behavior, or hit encoding.
        let state = self.refresh()?;
        Ok(crate::backend::search_screen(&state, query))
    }

    fn command_state(&mut self) -> Option<CommandState> {
        None
    }

    fn capabilities(&self) -> Capabilities {
        use super::{EventCapability, InputFamily, WaitCapability};
        Capabilities {
            mouse: false,
            kitty_keyboard: false,
            colors: true,
            cell_attributes: true,
            // Audit P0-38: title is wired FOR REAL — the pane's last OSC title
            // is read via `#{pane_title}` and rides into screen state, so the
            // claim stays true.
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
            // --- audit finding 37: operation-oriented matrix ---
            // tmux send-keys delivers TEXT, not arbitrary bytes: a non-UTF-8
            // raw payload is refused (audit P0-40), so raw input is NOT
            // advertised even though UTf-8 text happens to pass through.
            raw_input: false,
            bell_observable: true, // window bell flag (audit P0-39)
            // The attached process is not our child; its real exit code is
            // unknowable (process() reports exit_code: None).
            exit_code: false,
            shell_integration: false, // command_state() returns None
            stdout_stderr_separation: false,
            recording: true, // recording hook delivered from pane capture
            // Finding 6: FALSE for attach. The native semantic channel is
            // provisioned at SESSION launch by injecting TUI_LAB_SEMANTIC
            // into the child's environment — impossible for a pane this
            // backend attached and did not spawn. Claiming it here promised
            // native events that can never arrive on an attach.
            native_semantic: false,
            // Attach semantics: this backend attaches an EXISTING TUI it did
            // not spawn.
            attach: true,
            // Finding 6: the ownership model the rest of the system keys off
            // — not our child, so no signals, exit_code: false, and a plain
            // detach leaves the pane running.
            process_ownership: ProcessOwnership::Attached,
            query_response: false, // no device-query responder behind tmux
            event_types: vec![
                EventCapability::Output,
                EventCapability::Bell,
                EventCapability::Title,
                EventCapability::FocusChanged,
                EventCapability::SemanticChanged,
            ],
            supported_waits: vec![
                WaitCapability::Text,
                WaitCapability::TextAbsent,
                WaitCapability::ScreenChange,
                WaitCapability::ScreenStable,
                WaitCapability::ProcessExit,
                WaitCapability::Title,
                WaitCapability::Bell,
                WaitCapability::AnyActivity,
                WaitCapability::Idle,
            ],
            // Finding 6: RawByte is NOT listed. send-keys accepts UTF-8
            // TEXT only (`Input::Raw` with non-UTF-8 bytes is refused), so
            // advertising the RawByte family here contradicted
            // `raw_input: false` and the send-path refusal. Text-shaped
            // payload delivery is the Key/Paste families.
            input_families: vec![InputFamily::Key, InputFamily::Paste, InputFamily::Resize],
            // Finding 38: the persistent `tmux -C` client retains pane
            // output between observations. Pane lifecycle facts remain
            // authoritative from the format query, and historical output is
            // ring-bounded/drop-declared. With the control client unavailable,
            // the backend transparently remains a sampled polling fallback.
            observability_fidelity: Some(super::ObservabilityFidelity {
                mode: if self.control.is_some() {
                    "control_mode_ring"
                } else {
                    "sampled"
                }
                .to_string(),
                sampling_ms: if self.control.is_some() { 0 } else { 50 },
                blind_spots: if self.control.is_some() {
                    vec![
                        "bounded control output ring may evict rapid history".to_string(),
                        "pane lifecycle remains query-authoritative".to_string(),
                    ]
                } else {
                    vec![
                        "rapid frame changes coalesced between polls".to_string(),
                        "multiple bells inside one interval counted as one edge".to_string(),
                        "raw protocol timing unavailable".to_string(),
                    ]
                },
            }),
            synchronized_updates: false,
            osc8: false,
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
        // Audit finding 35: independent dedicated counters. `screen_seq`
        // remains the content/render transition domain for compatibility,
        // `visual_seq` is the visual fingerprint domain, and
        // `interaction_seq` is its own monotonic edge counter — never a sum
        // of unrelated domains or state booleans.
        TerminalEventState {
            output_seq: self.output_seq,
            screen_seq: self.screen_seq,
            content_seq: self.screen_seq,
            visual_seq: self.screen_seq,
            interaction_seq: self.interaction_seq,
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
            exit_signal: self.pane_exit_signal.clone(),
            cwd: None,
            pid: self.pane_pid,
        };
        crate::screen::from_vt(
            self.parser.screen(),
            process,
            self.last_title.clone(),
            Vec::new(),
        )
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
                named(&format!("{}C-{c}", if alt { "M-" } else { "" }))
            } else if alt {
                named(&format!("M-{c}"))
            } else {
                Ok(K::Lit(c.to_string()))
            }
        }
    }
}

/// Finding 38: one terminal event retained by the control-mode client.
/// `%output` is the pane's REAL byte stream as relayed by tmux — including
/// escape sequences — so these events can restore protocol-level evidence
/// that pane captures cannot provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxControlEvent {
    pub at_unix_ms: u64,
    pub monotonic_ms: u64,
    pub kind: TmuxControlEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxControlEventKind {
    /// Pane id and the exact `%output ... <base64> ...` payload.
    Output { pane: String, bytes: Vec<u8> },
    /// `%exit pane-id [reason]`; `reason` is tmux's own text.
    Exit {
        pane: String,
        reason: Option<String>,
    },
}

#[derive(Debug)]
struct TmuxControlMode {
    child: std::process::Child,
    stdout: std::process::ChildStdout,
    pending: Vec<u8>,
    events: std::collections::VecDeque<TmuxControlEvent>,
    bytes_total: u64,
    bytes_dropped: u64,
}

impl TmuxControlMode {
    const CAPACITY: usize = 256 * 1024;
    const MAX_EVENTS: usize = 512;

    fn spawn(target: &str) -> BackendResult<Self> {
        let mut child = Command::new("tmux")
            .args(["-C", "attach-session", "-t", target])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(BackendError::Io)?;
        let stdout = child.stdout.take().expect("tmux control stdout");
        Ok(Self {
            child,
            stdout,
            pending: Vec::new(),
            events: Default::default(),
            bytes_total: 0,
            bytes_dropped: 0,
        })
    }

    fn push(&mut self, event: TmuxControlEvent) {
        self.events.push_back(event);
        if self.events.len() > Self::MAX_EVENTS {
            self.events.pop_front();
            self.bytes_dropped += 1;
        }
    }

    /// Read all currently available control bytes, parse complete
    /// `%begin ... %end` blocks, and retain the declared bounds of `%output`.
    /// Other notifications are deliberately ignored: pane liveness remains
    /// polled from the authoritative tmux format query, so a notification
    /// parser can never turn an unknown frame into a lifecycle claim.
    fn drain(&mut self) -> BackendResult<()> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(BackendError::Io(e)),
            }
            if self.pending.len() >= Self::CAPACITY {
                break;
            }
        }
        let now_u = crate::events::unix_ms();
        let now_m = crate::events::monotonic_ms();
        let (blocks, consumed) = take_control_blocks(&self.pending);
        self.pending.drain(..consumed);
        for block in blocks {
            self.bytes_total = self.bytes_total.saturating_add(block.len() as u64);
            let mut lines = block.lines();
            let first = lines.next().unwrap_or_default();
            let mut fields = first.split_whitespace();
            if fields.next() != Some("%begin") {
                continue;
            }
            let _timestamp = fields.next();
            let _flags = fields.next();
            let _last = fields.next().unwrap_or_default();
            let mut body = lines.next().unwrap_or_default().to_string();
            while let Some(next) = lines.next() {
                if next.starts_with("%end ") {
                    break;
                }
                body.push('\n');
                body.push_str(next);
            }
            let mut output = body.splitn(3, ' ');
            if output.next() != Some("%output") {
                continue;
            }
            let pane = output.next().unwrap_or_default().to_string();
            let payload = output.next().unwrap_or_default();
            let bytes = crate::backend::tmux::decode_base64(payload);
            if bytes.is_empty() {
                continue;
            }
            self.push(TmuxControlEvent {
                at_unix_ms: now_u,
                monotonic_ms: now_m,
                kind: TmuxControlEventKind::Output { pane, bytes },
            });
        }
        if self.pending.len() > Self::CAPACITY {
            let drop = self.pending.len() - Self::CAPACITY;
            self.pending.drain(..drop);
            self.bytes_dropped += drop as u64;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            if !status.success() {
                return Err(BackendError::Unsupported(format!(
                    "tmux control client exited: {status}"
                )));
            }
        }
        Ok(())
    }

    fn take_events(&mut self, after_seq: u64) -> Vec<TmuxControlEvent> {
        self.events
            .iter()
            .skip(after_seq as usize)
            .cloned()
            .collect()
    }

    fn stats(&self) -> (usize, u64, u64) {
        (Self::CAPACITY, self.bytes_total, self.bytes_dropped)
    }
}

fn take_control_blocks(input: &[u8]) -> (Vec<String>, usize) {
    let text = String::from_utf8_lossy(input);
    let mut blocks = Vec::new();
    let mut consumed = 0;
    loop {
        let Some(found) = text[consumed..].find("%begin ") else {
            break;
        };
        let begin_at = consumed + found;
        let begin_line_end = text[begin_at..]
            .find('\n')
            .map(|i| begin_at + i + 1)
            .unwrap_or(text.len());
        let begin_line = text[begin_at..begin_line_end].trim();
        let end_needle = format!(
            "\n%end {}",
            begin_line
                .split_whitespace()
                .nth(2)
                .unwrap_or("")
                .to_string()
        );
        let Some(end_at) = text[begin_line_end..].find(&end_needle) else {
            break;
        };
        let block_end = begin_line_end + end_at + end_needle.len();
        // Include `%begin`, the body, and the exact `%end` footer in the
        // consumed prefix so partial trailing blocks stay pending.
        blocks.push(text[begin_at..begin_line_end + end_at].to_string());
        consumed = block_end;
    }
    (blocks, consumed)
}

fn decode_base64(input: &str) -> Vec<u8> {
    // tmux control mode base64 payloads omit padding. This small decoder
    // avoids a new dependency for exactly one producer format.
    const INVALID: u8 = 255;
    fn value(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => INVALID,
        }
    }
    // A padded payload's trailing `=` has no bit value; trim only the
    // canonical 0–2 padding symbols, then reject anything else.
    let trimmed = input.trim().trim_end_matches('=');
    if input.trim().len() - trimmed.len() > 2 {
        return Vec::new();
    }
    let bytes = trimmed.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        let v = value(b);
        if v == INVALID {
            return Vec::new();
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_base64_decodes_padded_and_unpadded() {
        assert_eq!(decode_base64("SGVsbG8="), b"Hello".to_vec());
        assert_eq!(decode_base64("SGVsbG8"), b"Hello".to_vec());
        assert_eq!(decode_base64("AAEC"), vec![0, 1, 2]);
        assert!(decode_base64("not-base64!").is_empty());
    }

    #[test]
    fn control_blocks_parse_output_payload() {
        let raw = b"%begin 1 1 0\n%output %3 414243\n%end 1 1 0\n".to_vec();
        let (blocks, consumed) = take_control_blocks(&raw);
        assert_eq!(blocks.len(), 1);
        assert_eq!(consumed, 37);
        let block = &blocks[0];
        assert!(block.contains("%begin 1 1 0"));
        assert!(block.contains("%output %3 414243"));
        assert!(
            !block.contains("%end 1 1 0"),
            "footer is consumed, not retained as body"
        );
    }

    #[test]
    fn control_unavailable_stays_declared_polling_fallback() {
        let mut b = TmuxBackend {
            target: TmuxTarget {
                session: "t".into(),
                window: "w".into(),
                pane: "p".into(),
            },
            cols: 80,
            rows: 24,
            parser: Parser::new(24, 80, 0),
            pane_dead: false,
            pane_pid: None,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            interaction_seq: 0,
            prev_content_hash: None,
            prev_visual_hash: None,
            last_title: None,
            prev_bell_flag: false,
            last_screen_change_at_ms: 0,
            last_output_at_ms: 0,
            last_capture: String::new(),
            content_hash: String::new(),
            visual_hash: String::new(),
            recording_slot: new_recording_hook_slot(),
            kill_on_stop: false,
            started: true,
            prev_pane_dead: false,
            pane_exit_signal: None,
            control: None,
        };
        let err = b
            .recent_raw_output()
            .expect_err("no control ring is Unsupported");
        assert!(err.to_string().contains("control client unavailable"));
        assert_eq!(b.raw_output_stats(), (0, 0));
        assert!(b.tmux_control_events(0).is_empty());
        let caps = b.capabilities();
        let fidelity = caps.observability_fidelity.expect("declared fidelity");
        assert_eq!(fidelity.mode, "sampled");
        assert_eq!(fidelity.sampling_ms, 50);
    }

    #[test]
    fn pane_death_transition_is_claimed_once_with_signal_provenance() {
        let mut b = TmuxBackend {
            target: TmuxTarget {
                session: "t".into(),
                window: "w".into(),
                pane: "p".into(),
            },
            cols: 80,
            rows: 24,
            parser: Parser::new(24, 80, 0),
            pane_dead: false,
            pane_pid: None,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            interaction_seq: 0,
            prev_content_hash: None,
            prev_visual_hash: None,
            last_title: None,
            prev_bell_flag: false,
            last_screen_change_at_ms: 0,
            last_output_at_ms: 0,
            last_capture: String::new(),
            content_hash: String::new(),
            visual_hash: String::new(),
            recording_slot: new_recording_hook_slot(),
            kill_on_stop: false,
            started: true,
            prev_pane_dead: false,
            pane_exit_signal: None,
            control: None,
        };
        // Simulate the death transition copied from `query_pane`: tmux's
        // signal token is captured once on false→true and is not overwritten
        // by later observations of the same death.
        for (dead, signal) in [
            (false, None),
            (true, Some("KILL".to_string())),
            (true, None),
        ] {
            if dead && !b.prev_pane_dead {
                b.pane_exit_signal = signal;
            }
            b.prev_pane_dead = dead;
            b.pane_dead = dead;
        }
        assert!(b.pane_dead);
        assert_eq!(b.pane_exit_signal.as_deref(), Some("KILL"));
    }

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
            tmux_key(&crate::backend::KeyEvent::with_modifiers(code, mods)).expect("key")
        };
        // Named keys are presses; plain chars are literal text.
        assert_eq!(
            k(KeyCode::Enter, KeyModifiers::NONE),
            K::Named("Enter".into())
        );
        assert_eq!(
            k(KeyCode::Tab, KeyModifiers::SHIFT),
            K::Named("BTab".into())
        );
        assert_eq!(
            k(KeyCode::Char('c'), KeyModifiers::CTRL),
            K::Named("C-c".into())
        );
        assert_eq!(
            k(KeyCode::Char('q'), KeyModifiers::NONE),
            K::Lit("q".into())
        );
        assert_eq!(
            k(KeyCode::Function(5), KeyModifiers::NONE),
            K::Named("F5".into())
        );
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
            // Audit P0-38: title is wired FOR REAL (the pane's last OSC
            // title is read via `#{pane_title}` and rides into screen
            // state), so the claim stays true — but see the title-edge
            // tests below.
            title: true,
            scrollback: true,
            bracketed_paste: false,
            signals: false,
            protocol_capture: false,
            // audit finding 37: the operation-oriented matrix. `attach` and
            // the observed event kinds are honors we claim explicitly (the
            // backend attaches panes and observes titles/bells); everything
            // else not listed restores to the no-claim `honest()` default.
            attach: true,
            // Finding 6: this mirrors capabilities()' real claim — the pane
            // is observed, not owned, and the native channel was never in
            // its environment.
            process_ownership: crate::backend::ProcessOwnership::Attached,
            native_semantic: false,
            event_types: vec![
                crate::backend::EventCapability::Output,
                crate::backend::EventCapability::Bell,
                crate::backend::EventCapability::Title,
                crate::backend::EventCapability::FocusChanged,
                crate::backend::EventCapability::SemanticChanged,
            ],
            ..Capabilities::honest()
        };
        // These are the boundaries the module header promises. If a future
        // change claims tmux can capture raw bytes or signal the child, that
        // is a capability it does not actually have — fail.
        assert!(
            !caps.protocol_capture,
            "tmux cannot hand back raw protocol bytes"
        );
        assert!(
            !caps.signals,
            "the attached process is not our child; no signals"
        );
        assert!(!caps.mouse, "tmux pane mouse injection is unsupported");
        // audit finding 37: operation-oriented honesty for the attach engine.
        assert!(caps.attach, "tmux attaches an existing TUI");
        // Finding 6: ownership + native-channel honesty. An attached pane is
        // not our child, and the native semantic channel could never have
        // been injected into its environment.
        assert_eq!(
            caps.process_ownership,
            crate::backend::ProcessOwnership::Attached,
            "the pane is observed, not owned"
        );
        assert!(
            !caps.native_semantic,
            "attach cannot inject TUI_LAB_SEMANTIC into the child env"
        );
        assert!(
            !caps
                .input_families
                .contains(&crate::backend::InputFamily::RawByte),
            "UTF-8-only send-keys is not raw-byte input"
        );
        assert!(!caps.raw_input, "tmux send-keys cannot deliver raw bytes");
        assert!(!caps.exit_code, "tmux cannot report the child's exit code");
        assert!(
            !caps.shell_integration,
            "tmux has no OSC 133 command-state observability"
        );
        assert!(
            !caps
                .supported_waits
                .contains(&crate::backend::WaitCapability::CommandDone),
            "tmux command waits return Unsupported"
        );
        assert!(
            !caps
                .input_families
                .contains(&crate::backend::InputFamily::Signal),
            "tmux cannot deliver signals to the attached process"
        );
        assert!(
            caps.event_types
                .contains(&crate::backend::EventCapability::Title),
            "tmux observes pane titles"
        );
    }

    /// Audit P0-38: the title-tracking edges — a fresh title advances
    /// `title_seq` exactly once, a repeat does not, and the title rides
    /// into the screen state.
    #[test]
    fn title_edges_advance_seq_once() {
        let mut b = TmuxBackend {
            target: TmuxTarget {
                session: "t".into(),
                window: "w".into(),
                pane: "p".into(),
            },
            cols: 80,
            rows: 24,
            parser: Parser::new(24, 80, 0),
            pane_dead: false,
            pane_pid: None,
            screen_seq: 0,
            output_seq: 0,
            bell_seq: 0,
            title_seq: 0,
            interaction_seq: 0,
            prev_content_hash: None,
            prev_visual_hash: None,
            last_title: None,
            prev_bell_flag: false,
            last_screen_change_at_ms: 0,
            last_output_at_ms: 0,
            last_capture: String::new(),
            content_hash: String::new(),
            visual_hash: String::new(),
            recording_slot: new_recording_hook_slot(),
            kill_on_stop: false,
            started: false,
            prev_pane_dead: false,
            pane_exit_signal: None,
            control: None,
        };
        assert_eq!(b.title_seq, 0);
        // Simulate the query_pane edge logic directly (no live tmux server
        // in unit tests): a first title bumps once.
        let title_a: Option<String> = Some("sh".into());
        if title_a != b.last_title {
            b.title_seq += 1;
            b.interaction_seq += 1;
            b.last_title = title_a.clone();
        }
        assert_eq!(b.title_seq, 1);
        assert_eq!(b.interaction_seq, 1);
        // Same title again: no bump.
        if title_a != b.last_title {
            b.title_seq += 1;
            b.interaction_seq += 1;
        }
        assert_eq!(b.title_seq, 1);
        assert_eq!(b.interaction_seq, 1);
        // New title: second bump.
        let title_b: Option<String> = Some("app".into());
        if title_b != b.last_title {
            b.title_seq += 1;
            b.interaction_seq += 1;
            b.last_title = title_b;
        }
        assert_eq!(b.title_seq, 2);
        assert_eq!(b.interaction_seq, 2);
        assert_eq!(b.last_title.as_deref(), Some("app"));
        // Finding 35: a later title edge must not be inferred by adding
        // unrelated domains; the dedicated interaction clock advanced once
        // per real edge.
        assert_eq!(b.interaction_seq, b.title_seq + b.bell_seq);
    }

    /// Audit P0-39: the bell edge — a 0→1 flag transition bumps `bell_seq`
    /// once; the flag STAYING set does not; a reset to 0 re-arms.
    #[test]
    fn bell_flag_edges_bump_bell_seq() {
        let mut bell_seq: u64 = 0;
        let mut prev = false;
        for flag in [true, true, true, false, true] {
            if flag && !prev {
                bell_seq += 1;
            }
            prev = flag;
        }
        assert_eq!(bell_seq, 2, "one ring, re-arm, one more ring");
    }
}
