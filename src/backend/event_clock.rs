//! Event sequencing clock for the portable PTY backend (god-object round
//! 2, G2).
//!
//! The five sequence counters (`output_seq` / `screen_seq` /
//! `content_seq` / `bell_seq` / `title_seq`), the wall-clock and
//! monotonic stamps of the latest output / screen change, and the bounded
//! per-change log move OUT of `PortablePtyBackend` into this holder. Pure
//! clock policy: when each counter bumps, what the bounded log retains.
//! No PTY, no parser, no threads.
//!
//! The backend keeps its single canonical `pump()`; it calls
//! [`BackendEventClock::on_output`] / [`BackendEventClock::on_screen_change`]
//! at the same instants it previously mutated the flat fields, and reads
//! waits/capabilities/event-state through the clock's accessors. The
//! query-answer bookkeeping (`last_query_class` /
//! `last_query_answered_seq`) moves in too: it is sequencing state the
//! pump promotes at the moment the answer bytes actually leave for the
//! PTY (item 22).

use std::time::Instant;

/// Item 26: bounded log of `(screen_seq, unix_ms)` at each screen change,
/// so an action's FIRST frame instant is measurable even when several
/// changes followed it (the scalar `last_screen_change_at_ms` only
/// remembers the last). Capped; on overflow the head is dropped.
pub const SCREEN_CHANGE_LOG_CAP: usize = 256;

/// Sequencing clock + per-change log + query-answer bookkeeping for one
/// backend instance.
pub struct BackendEventClock {
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
    screen_change_log: Vec<(u64, u64)>,
    // Monotonic Instants for fine-grained wait timing (monotonic clock,
    // unlike SystemTime which can jump). These are updated whenever the
    // corresponding event is recorded.
    last_output_instant: Instant,
    last_screen_change_instant: Instant,
}

impl BackendEventClock {
    /// A clock starting now (so ScreenStable/Idle waits entered before the
    /// first byte are honestly not-yet-quiet).
    pub fn new() -> Self {
        let now = Instant::now();
        BackendEventClock {
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
            last_output_instant: now,
            last_screen_change_instant: now,
        }
    }

    /// Record one drained output chunk.
    pub fn on_output(&mut self) {
        self.output_seq += 1;
        self.last_output_at_ms = super::line_types::now_ms();
        self.last_output_instant = Instant::now();
    }

    /// Record a screen mutation at the current `screen_seq` (the caller
    /// bumps the counter via [`Self::bump_screen`] first). Appends to the
    /// bounded per-change log, dropping the head on overflow.
    pub fn on_screen_change(&mut self) {
        self.last_screen_change_at_ms = super::line_types::now_ms();
        self.last_screen_change_instant = Instant::now();
        if self.screen_change_log.len() == SCREEN_CHANGE_LOG_CAP {
            self.screen_change_log.remove(0);
        }
        self.screen_change_log
            .push((self.screen_seq, self.last_screen_change_at_ms));
    }

    /// Text-only change (fingerprint unchanged): bump `content_seq`.
    pub fn bump_content(&mut self) {
        self.content_seq += 1;
    }

    /// Fingerprint change (text or style): bump `screen_seq`.
    pub fn bump_screen(&mut self) {
        self.screen_seq += 1;
    }

    /// Pull bell/title counters out of the parser callbacks (shared by
    /// `state()`, `wait()` and `send_input`): adopt them when the
    /// callbacks ran ahead.
    pub fn sync_counters(&mut self, audible_bells: u64, title_seq: u64) {
        if audible_bells > self.bell_seq {
            self.bell_seq = audible_bells;
        }
        if title_seq > self.title_seq {
            self.title_seq = title_seq;
        }
    }

    /// Item 22: promote the pending query class at the moment its answer
    /// bytes were written back to the PTY — that write is the measured
    /// "answer sent at".
    pub fn note_query_answered(&mut self, class: &'static str, answered_seq: u64) {
        self.last_query_class = Some(class);
        self.last_query_answered_seq = answered_seq;
    }

    /// Item 22: the most recent answered query — `(class, counter)`.
    pub fn last_query_answer(&self) -> (Option<&'static str>, u64) {
        (self.last_query_class, self.last_query_answered_seq)
    }

    /// Item 26: `(screen_seq, unix_ms)` for every screen change at/after
    /// `after_seq`, oldest first — the evidence an action's FIRST frame
    /// latency is derived from. Empty when nothing changed since.
    pub fn changes_since(&self, after_seq: u64) -> Vec<(u64, u64)> {
        self.screen_change_log
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .copied()
            .collect()
    }

    /// Whether the screen has been quiet for at least `quiet_for`
    /// (monotonic; the basis of ScreenStable waits).
    pub fn screen_quiet_for(&self, quiet_for: std::time::Duration) -> bool {
        self.last_screen_change_instant.elapsed() >= quiet_for
    }

    /// Whether PTY output has been quiet for at least `quiet_for`
    /// (monotonic; the basis of Idle waits).
    pub fn output_quiet_for(&self, quiet_for: std::time::Duration) -> bool {
        self.last_output_instant.elapsed() >= quiet_for
    }

    /// The current screen sequence (wait anchors, wait outcomes).
    pub fn screen_seq(&self) -> u64 {
        self.screen_seq
    }

    /// The current output sequence (wait anchors, wait outcomes).
    pub fn output_seq(&self) -> u64 {
        self.output_seq
    }

    /// The current bell sequence (Bell waits, capability honesty).
    pub fn bell_seq(&self) -> u64 {
        self.bell_seq
    }

    /// The current title sequence (Title capability honesty).
    pub fn title_seq(&self) -> u64 {
        self.title_seq
    }

    /// Review P0 (AnyObservableChange): screen + bell + title — a pure sum
    /// so any single edge advancing any component advances it.
    pub fn interaction_seq(&self) -> u64 {
        self.screen_seq + self.bell_seq + self.title_seq
    }

    /// The flat event-state projection (spec section 1) the session layer
    /// reads for anchoring and event diffs.
    pub fn event_state(&self, command_seq: u64) -> super::TerminalEventState {
        super::TerminalEventState {
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
            command_seq,
        }
    }

    /// Reset to a fresh stream (backend start). Counters restart: the
    /// output stream being observed is a new one.
    pub fn clear(&mut self) {
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
        let now = Instant::now();
        self.last_output_instant = now;
        self.last_screen_change_instant = now;
    }
}

impl Default for BackendEventClock {
    fn default() -> Self {
        BackendEventClock::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn counters_start_zero_and_instants_start_now() {
        let clock = BackendEventClock::new();
        assert_eq!(clock.output_seq(), 0);
        assert_eq!(clock.screen_seq(), 0);
        assert_eq!(clock.interaction_seq(), 0);
        // A fresh clock's instants were just initialized, so it is NOT yet
        // quiet for a positive interval (same honesty as the old flat
        // fields initializing at `Instant::now()`); a zero interval is.
        assert!(!clock.screen_quiet_for(Duration::from_millis(1)));
        assert!(clock.screen_quiet_for(Duration::ZERO));
        assert!(!clock.output_quiet_for(Duration::from_millis(1)));
        assert!(clock.output_quiet_for(Duration::ZERO));
        // Wall stamps start at epoch-0 like the old fields.
        let st = clock.event_state(0);
        assert_eq!(st.last_output_at, 0);
        assert_eq!(st.last_screen_change_at, 0);
    }

    #[test]
    fn output_and_screen_edges_advance_their_counters() {
        let mut clock = BackendEventClock::new();
        clock.on_output();
        clock.on_output();
        assert_eq!(clock.output_seq(), 2);
        clock.bump_screen();
        clock.on_screen_change();
        assert_eq!(clock.screen_seq(), 1);
        // content bumps independently of screen.
        clock.bump_content();
        let st = clock.event_state(7);
        assert_eq!(st.content_seq, 1);
        assert_eq!(st.command_seq, 7);
        assert_eq!(st.visual_seq, 1);
        assert_eq!(st.interaction_seq, 1);
        assert!(st.last_screen_change_at > 0, "wall stamp recorded");
        // The change landed in the log at the current seq.
        assert_eq!(clock.changes_since(0), vec![(1, st.last_screen_change_at)]);
        assert!(clock.changes_since(1).is_empty());
    }

    #[test]
    fn bounded_change_log_drops_the_head() {
        let mut clock = BackendEventClock::new();
        for _ in 0..(SCREEN_CHANGE_LOG_CAP + 5) {
            clock.bump_screen();
            clock.on_screen_change();
        }
        let log = clock.changes_since(0);
        assert_eq!(log.len(), SCREEN_CHANGE_LOG_CAP, "head was dropped");
        // The oldest surviving entry is the 6th change (seq 6): 1..=5 gone.
        assert_eq!(log[0].0, 6);
        let last = *log.last().unwrap();
        assert_eq!(last.0, SCREEN_CHANGE_LOG_CAP as u64 + 5);
    }

    #[test]
    fn sync_counters_only_advance() {
        let mut clock = BackendEventClock::new();
        clock.sync_counters(3, 0);
        assert_eq!(clock.bell_seq(), 3);
        // A stale callback snapshot must never move the clock backwards.
        clock.sync_counters(1, 2);
        assert_eq!(clock.bell_seq(), 3);
        assert_eq!(clock.title_seq(), 2);
        assert_eq!(clock.interaction_seq(), 5);
    }

    #[test]
    fn query_answer_promotion_is_measured_once() {
        let mut clock = BackendEventClock::new();
        assert_eq!(clock.last_query_answer(), (None, 0));
        clock.note_query_answered("da1", 4);
        assert_eq!(clock.last_query_answer(), (Some("da1"), 4));
        clock.note_query_answered("dsr", 5);
        assert_eq!(clock.last_query_answer(), (Some("dsr"), 5));
    }

    #[test]
    fn clear_restarts_the_stream() {
        let mut clock = BackendEventClock::new();
        clock.on_output();
        clock.bump_screen();
        clock.on_screen_change();
        clock.note_query_answered("da1", 1);
        clock.clear();
        assert_eq!(clock.output_seq(), 0);
        assert_eq!(clock.screen_seq(), 0);
        assert_eq!(clock.last_query_answer(), (None, 0));
        assert!(clock.changes_since(0).is_empty());
        assert!(!clock.screen_quiet_for(Duration::from_millis(1)));
        assert!(clock.screen_quiet_for(Duration::ZERO));
    }
}
