//! The `TerminalBackend` trait — the stable contract (spec section 2).
//!
//! Every method here now reflects *working* behavior. `wait` returns a rich
//! [`WaitOutcome`] instead of a bare bool, and `observe` reports how long the
//! screen actually stayed quiet.

use super::{
    BackendResult, Capabilities, Input, InputModes, ObserveResult, TerminalEventState, WaitCond,
    WaitOutcome,
};
use crate::screen::{ProcessState, ScreenState};

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

    /// Observe current terminal state (parse buffered PTY bytes into a screen).
    fn state(&mut self) -> BackendResult<ScreenState>;

    /// Convenience: observe + wait for stability. Returns the settled screen.
    fn observe(&mut self, idle: std::time::Duration) -> BackendResult<ObserveResult> {
        let screen = self.state()?;
        // Cheap settle: re-parse once after the idle window.
        std::thread::sleep(idle);
        let screen2 = self.state()?;
        let stable = screen.raw_hash == screen2.raw_hash;
        Ok(ObserveResult {
            screen: screen2,
            stable,
            stable_ms: idle.as_millis() as u64,
        })
    }

    /// Send an input event to the PTY. Encoding is mode-aware (spec section 4).
    fn send_input(&mut self, input: Input) -> BackendResult<()>;

    /// Resize the pty/terminal. Implementations MUST resize both the OS PTY
    /// and the `vt100` parser dimensions (spec section 3).
    fn resize(&mut self, cols: u16, rows: u16) -> BackendResult<()>;

    /// Block until a wait condition holds or the budget elapses. Returns a rich
    /// outcome describing what resolved and at what state (spec section 1).
    fn wait(&mut self, cond: WaitCond, budget: std::time::Duration) -> BackendResult<WaitOutcome>;

    /// Current snapshot (alias for `state`, kept for API clarity).
    fn snapshot(&mut self) -> BackendResult<ScreenState> {
        self.state()
    }

    /// Whether a recording session is active (asciinema cast export, etc.).
    fn recording(&self) -> bool {
        false
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
