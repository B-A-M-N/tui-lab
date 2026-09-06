//! Terminal emulator state for the portable PTY backend (god-object
//! round 2, G2).
//!
//! The `vt100::Parser` (with the protocol callbacks), the structure-hash
//! normalization policy, and the materialized scrollback cache move OUT
//! of `PortablePtyBackend` into this holder. Emulator policy: how bytes
//! enter the grid, how history is paged out of the parser without
//! disturbing the live view, which volatile patterns structure hashes
//! ignore.
//!
//! The backend keeps its single canonical `pump()`; it calls
//! [`TerminalEmulator::feed`] at the one ingestion point and reads
//! snapshots/modes through the emulator's accessors. Protocol state
//! (callbacks' fields) stays in [`super::protocol`]; the emulator holds
//! the parser that carries it and hands out callbacks access.

use vt100::Parser;

use super::protocol::BackendCallbacks;

/// The parser + normalization policy + scrollback state for one backend
/// instance.
pub(super) struct TerminalEmulator {
    parser: Parser<BackendCallbacks>,
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
    /// capability honesty (promoted only by real history).
    scrollback_seen: bool,
}

impl TerminalEmulator {
    pub(super) fn new(cols: u16, rows: u16) -> Self {
        TerminalEmulator {
            parser: Parser::new_with_callbacks(rows, cols, 10_000, BackendCallbacks::default()),
            normalization_policy: std::sync::Arc::new(crate::screen::NormalizationPolicy::default()),
            scrollback_cache: Vec::new(),
            scrollback_seen: false,
        }
    }

    /// Rebuild the parser for a fresh child (`start()`). Callback state
    /// resets with it — a new application session must not inherit the
    /// old one's observed protocol state.
    pub(super) fn reset(&mut self, cols: u16, rows: u16) {
        self.parser = Parser::new_with_callbacks(rows, cols, 10_000, BackendCallbacks::default());
        self.scrollback_cache.clear();
        self.scrollback_seen = false;
    }

    /// Resize the grid (OS PTY resize flows through here too).
    pub(super) fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Feed child bytes into the grid (the canonical ingestion point —
    /// the backend's `pump()` is the only caller besides the probe).
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Item 48: install a contract-derived normalization policy. Affects
    /// every subsequent `state()` / `observe()` structure hash.
    pub(super) fn set_normalization_policy(
        &mut self,
        policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
        self.normalization_policy = policy;
    }

    /// The installed policy (structure-hash construction).
    pub(super) fn normalization_policy(
        &self,
    ) -> &std::sync::Arc<crate::screen::NormalizationPolicy> {
        &self.normalization_policy
    }

    /// Read-only screen access (fingerprints, mode queries, snapshots).
    pub(super) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Read-only callback state (protocol observation).
    pub(super) fn callbacks(&self) -> &BackendCallbacks {
        self.parser.callbacks()
    }

    /// Mutable callback state (response draining, counter sync).
    pub(super) fn callbacks_mut(&mut self) -> &mut BackendCallbacks {
        self.parser.callbacks_mut()
    }

    /// Wave F item 53: the scrollback materialized at the last refresh.
    pub(super) fn scrollback_cache(&self) -> &[String] {
        &self.scrollback_cache
    }

    /// Whether any scrollback row was ever captured.
    pub(super) fn scrollback_seen(&self) -> bool {
        self.scrollback_seen
    }

    /// Wave F item 53: copy the parser's scrollback rows into the cache.
    /// The vt100 API exposes history only by scrolling the view
    /// (`set_scrollback`); the offset is paged to each position, the
    /// visible top row copied, and the original offset restored — the
    /// live screen is untouched when this returns.
    ///
    /// History *length* is discovered by probing: `set_scrollback` clamps
    /// to the buffer size, so a huge request returns the actual length in
    /// `screen.scrollback()` (which is the *offset*, not the size, in the
    /// normal view).
    pub(super) fn refresh_scrollback(&mut self, cols: u16) {
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
                .rows(0, cols)
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
}
