//! The `TerminalBackend` trait — the stable contract (spec section 2).
//!
//! Every method here now reflects *working* behavior. `wait` returns a rich
//! [`WaitOutcome`] instead of a bare bool, and `observe` reports how long the
//! screen actually stayed quiet.

use super::{
    BackendError, BackendResult, Capabilities, CommandState, Input, InputModes, ObserveResult,
    SearchHit, TerminalEventState, WaitCond, WaitOutcome,
};
use crate::screen::{ProcessState, ScreenState};
use std::time::Duration;

/// What actually happened while a backend brought its target up (P1-41).
/// This replaces arbitrary fixed sleeps as the readiness authority: a blank
/// TUI is valid, and a deadline without output is not success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    /// Child/transport spawned successfully.
    Spawned,
    /// At least one byte/output chunk arrived.
    FirstOutput,
    /// A rendered/synthetic screen was materialized.
    FirstRender,
    /// The first materialized frame remained unchanged on a second pump.
    StableFrame,
    /// The child was already dead when checked.
    ProcessExited,
    /// The readiness deadline elapsed before a stronger phase.
    Deadline,
}

impl StartupPhase {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::FirstOutput => "first_output",
            Self::FirstRender => "first_render",
            Self::StableFrame => "stable_frame",
            Self::ProcessExited => "process_exited",
            Self::Deadline => "deadline",
        }
    }
}

/// Evidence-shaped startup result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartupOutcome {
    pub phase: StartupPhase,
    /// Wall-clock correlation time.
    pub at_unix_ms: u64,
    /// Monotonic process-start milliseconds.
    pub monotonic_ms: u64,
    /// How long readiness detection took.
    pub elapsed_ms: u64,
    /// Whether a render was observed (may be legitimately blank).
    pub render_observed: bool,
}

/// The typed result of one transport dispatch (audit findings 1/4): the
/// backend classifies whether a write was entered and preserves the original
/// error. This is the ONE place the write boundary is decided; the executor
/// maps it onto [`crate::execution::DispatchStatus`] without re-inferring.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// `Ok(())` = the complete payload was written. `Err` carries the
    /// backend's original error verbatim.
    pub result: BackendResult<()>,
    /// Whether the transport write was ENTERED. `true` only when partial
    /// delivery cannot be ruled out; encoding/preflight/unsupported
    /// failures happen before any byte could land and report `false`.
    pub write_entered: bool,
}

impl DispatchOutcome {
    /// A failure proven to have happened before the write boundary
    /// (preflight/encoding/unsupported failures).
    pub fn failed_before_write(err: BackendError) -> Self {
        DispatchOutcome {
            result: Err(err),
            write_entered: false,
        }
    }

    /// A failure at or after the write boundary (an I/O error surfaced by
    /// the write itself, where partial delivery cannot be ruled out).
    pub fn failed_in_write(err: BackendError) -> Self {
        DispatchOutcome {
            result: Err(err),
            write_entered: true,
        }
    }

    /// A successful dispatch.
    pub fn ok() -> Self {
        DispatchOutcome {
            result: Ok(()),
            write_entered: false,
        }
    }
}

pub trait TerminalBackend: Send {
    /// Start the target process under a PTY at the given size.
    fn start(
        &mut self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> BackendResult<()>;

    /// Stop the backend, killing the child (and its process group) if alive.
    fn stop(&mut self) -> BackendResult<()>;

    /// Evidence-shaped readiness state for the current generation.
    /// Backends that track output/screen counters should override this;
    /// the default reports only that the transport reached `start` success.
    fn startup_outcome(&mut self) -> StartupOutcome {
        StartupOutcome {
            phase: StartupPhase::Spawned,
            at_unix_ms: crate::events::unix_ms(),
            monotonic_ms: crate::events::monotonic_ms(),
            elapsed_ms: 0,
            render_observed: false,
        }
    }

    /// Observe current terminal state (parse buffered PTY bytes into a screen).
    fn state(&mut self) -> BackendResult<ScreenState>;

    /// Convenience: observe + wait for stability. Returns the settled screen.
    ///
    /// The default is a *fallback only* (a bounded quiet-poll, no fixed sleep
    /// longer than one poll interval). Backends with an event sequencer MUST
    /// override this so ordinary `session.observe(...)` synchronizes on real
    /// terminal events (audit item 4).
    fn observe(&mut self, idle: std::time::Duration) -> BackendResult<ObserveResult> {
        // Fallback: bounded quiet-poll using state snapshots. Overridden by
        // PortablePtyBackend with the event-sequenced wait machinery.
        let start = std::time::Instant::now();
        let _ = self.state()?;
        let quiet_budget = idle.max(Duration::from_millis(30));
        let out = self.wait(
            WaitCond::ScreenStable {
                quiet_for: quiet_budget.min(Duration::from_millis(120)),
                after_screen_seq: None,
            },
            quiet_budget + Duration::from_millis(500),
        )?;
        Ok(ObserveResult {
            screen: out.state,
            stable: out.met,
            stable_ms: start.elapsed().as_millis() as u64,
        })
    }

    /// Send an input event to the PTY. Encoding is mode-aware (spec section 4).
    fn send_input(&mut self, input: Input) -> BackendResult<()>;

    /// Typed dispatch (audit findings 1/4): the backend classifies whether
    /// the transport write was entered, so the executor can prove "no bytes
    /// landed" for preflight failures and must treat I/O failures at the
    /// write boundary as partial-unknown. The default uses the backend's
    /// `supported_inputs` capability family to classify preflight
    /// unsupported errors: only a backend that overrides with exact
    /// write-boundary knowledge can claim `FailedBeforeWrite` for an I/O
    /// path. Spawning backends (portable/line/pipe) override this with
    /// their real boundary; tmux uses the command-level all-or-nothing
    /// contract.
    fn dispatch(&mut self, input: Input) -> DispatchOutcome {
        match self.send_input(input) {
            Ok(()) => DispatchOutcome::ok(),
            Err(e @ BackendError::Unsupported(_)) => DispatchOutcome::failed_before_write(e),
            Err(e) => DispatchOutcome::failed_in_write(e),
        }
    }

    /// Resize the pty/terminal. Implementations MUST resize both the OS PTY
    /// and the `vt100` parser dimensions (spec section 3).
    fn resize(&mut self, cols: u16, rows: u16) -> BackendResult<()>;

    /// Block until a wait condition holds or the budget elapses. Returns a rich
    /// outcome describing what resolved and at what state (spec section 1).
    fn wait(&mut self, cond: WaitCond, budget: std::time::Duration) -> BackendResult<WaitOutcome>;

    /// Causality-explicit wait: capture the event state *before* an action,
    /// send the action, then call this with that baseline. Stability conditions
    /// then require a screen change with sequence > baseline.screen_seq (or
    /// output > baseline.output_seq for [`WaitCond::Idle`]) followed by the
    /// quiet interval. This is the canonical action→settle primitive; `wait`
    /// remains for generic conditions (audit items 1/2).
    fn wait_after(
        &mut self,
        baseline: TerminalEventState,
        cond: WaitCond,
        budget: std::time::Duration,
    ) -> BackendResult<WaitOutcome> {
        self.wait(cond.anchored_to(&baseline), budget)
    }

    /// Attach or detach the raw PTY recording hook (audit item 24). `None`
    /// detaches. The default ignores it (backends without a byte boundary).
    fn set_recording_hook(&mut self, _hook: super::RecordingHookSlot) {}

    /// Item 48: install the normalization policy used when computing
    /// structure hashes. A contract's `volatile_patterns` are merged on top
    /// of the built-in conservative classes. The default ignores it (the
    /// built-in default stays in effect).
    fn set_normalization_policy(
        &mut self,
        _policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
    }

    /// Wave G item 77: when set, the next `start()` clears the inherited
    /// environment before applying the caller-supplied pairs — the `clean`
    /// and `strict` isolation profiles. The default ignores it (local
    /// inheritance stays in effect), so backends without env control are
    /// honest about it rather than silently inheriting.
    fn set_clear_env(&mut self, _clear: bool) {}

    /// Current snapshot (alias for `state`, kept for API clarity).
    fn snapshot(&mut self) -> BackendResult<ScreenState> {
        self.state()
    }

    /// Wave F item 53: true scrollback — lines the application scrolled off
    /// the top of the viewport, oldest first. Backends that cannot retain
    /// history return an empty vec (and `Capabilities.scrollback` stays
    /// `false`); backends that can MUST return the real rows, never a
    /// truncated ring silently.
    fn scrollback_lines(&mut self) -> BackendResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// Wave-2 (protocol diagnostics): the most recent raw output bytes,
    /// oldest first, from a bounded ring. This is the child's REAL byte
    /// stream — escape sequences, OSC, DCS and all — so the protocol
    /// decoder can answer "what did this TUI actually emit?". The ring is
    /// bounded (`recent_raw_output_capacity`); when output exceeds it the
    /// head is dropped and `recent_raw_output_dropped` reports how many
    /// bytes were lost, so a caller never mistakes a partial window for the
    /// whole stream. Default: not retained (honest empty + dropped=0 is
    /// still distinguishable through the capacity=0 report).
    fn recent_raw_output(&mut self) -> BackendResult<Vec<u8>> {
        Ok(Vec::new())
    }

    /// Capacity of the raw-output ring (0 = not retained), and how many
    /// bytes were dropped off its head so far. Returns `(capacity, dropped)`.
    fn raw_output_stats(&mut self) -> (usize, u64) {
        (0, 0)
    }

    /// The absolute byte range the retained raw-output window covers in the
    /// child's output stream: `(start, end_exclusive)`. `(0, 0)` when the
    /// engine retains no raw bytes (re-review item 19: transaction-citable
    /// protocol ranges survive head eviction).
    fn raw_window_range(&mut self) -> (u64, u64) {
        (0, 0)
    }

    /// Item 22: the responder's most recent delivered answer —
    /// `(class, answered_counter)`; `(None, 0)` from engines without a
    /// device-query responder. Measured at the write that delivered the
    /// bytes back to the PTY. (G3: replaces the session layer's
    /// downcast to the concrete portable engine.)
    fn last_query_answer(&mut self) -> (Option<&'static str>, u64) {
        (None, 0)
    }

    /// Item 22: measured conformance probe — feed `query` through the same
    /// parser the child's output uses and return the responder's exact
    /// answer bytes `(class, answer)`. Nothing reaches the child; nothing
    /// enters the output ring. `(None, empty)` from engines without a
    /// responder.
    fn probe_query_response(&mut self, _query: &[u8]) -> (Option<&'static str>, Vec<u8>) {
        (None, Vec::new())
    }

    /// Item 26: `(screen_seq, unix_ms)` for every screen change at/after
    /// `after_seq`, oldest first — the measured evidence for an action's
    /// first-frame latency. Empty when the engine keeps no per-change log.
    fn screen_changes_since(&mut self, _after_seq: u64) -> Vec<(u64, u64)> {
        Vec::new()
    }

    /// Audit finding 45: `(screen_seq, monotonic_ms)` for every screen
    /// change at/after a sequence. This is the authoritative latency
    /// domain; the wall-clock log exists only for human correlation.
    fn screen_monotonic_changes_since(&mut self, _after_seq: u64) -> Vec<(u64, u64)> {
        Vec::new()
    }

    /// Finding 38: pane output retained by a backend's persistent event
    /// transport, at/after the caller's sequence. Empty from polling-only
    /// engines; the capability matrix/fidelity object remains the source of
    /// truth about whether this transport is active.
    fn tmux_control_events(
        &mut self,
        _after_seq: u64,
    ) -> Vec<crate::backend::tmux::TmuxControlEvent> {
        Vec::new()
    }

    /// Wave-2 (streams): genuine stdout/stderr line separation —
    /// `(stdout, stderr)`. `(empty, empty)` on engines that interleave by
    /// construction (a single PTY carries both). (G3: replaces the
    /// session layer's downcast to the concrete pipe engine.)
    fn separated_streams(&mut self) -> (Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// Downcast hook for engine-specific capability surfaces (the pipe
    /// backend's stdout/stderr separation). Engines return `self`.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Wave F item 53: search viewport + scrollback for a case-insensitive
    /// substring. Returns every hit with its region and (for viewport hits)
    /// cell coordinates so a caller can click it. Default searches the
    /// viewport only — correct but incomplete; scrollback-capable backends
    /// override to cover history too.
    fn search(&mut self, query: &str) -> BackendResult<Vec<SearchHit>> {
        let screen = self.state()?;
        Ok(crate::backend::search_screen(&screen, query))
    }

    /// Wave F item 54: the shell-integration phase of the foreground command
    /// (OSC 133). `None` when the application emits no shell integration —
    /// callers must treat command waits as honestly unsatisfiable then.
    fn command_state(&mut self) -> Option<CommandState> {
        None
    }

    /// Whether a recording session is active (asciinema cast export, etc.).
    fn recording(&self) -> bool {
        false
    }

    /// Whether `stop()` will KILL the observed process, or merely detach
    /// from it (audit finding 6). Only the tmux attach backend detaches
    /// without killing (its pane predates the session); every spawning
    /// backend owns the child and stops it, so the default is true.
    fn kill_on_stop(&self) -> bool {
        true
    }

    /// Capability flags for this backend/session. Must describe working
    /// behavior, not intended behavior (spec section 9).
    fn capabilities(&self) -> Capabilities;

    /// Process liveness/exit info.
    fn process(&mut self) -> ProcessState;

    /// Current negotiated input modes, derived from parsed terminal state
    /// (spec section 4). Used by the caller to know what was actually observed.
    fn input_modes(&self) -> InputModes;

    /// Monotonic per-event counters (spec section 1). Used for edge-triggered
    /// waits and for caller-side change detection.
    fn event_state(&self) -> TerminalEventState;
}
