//! Event-stream bookkeeping for one session (god-object round 2, G4).
//!
//! The per-session terminal event queue (Wave B item 12), the
//! per-consumer observation cursors (Wave B item 13), and the absorption
//! watermarks — the reader-thread ingest facts, the responder's answered
//! counter, the native-channel cursor, and the per-session anchor
//! counter — move OUT of the flat `Session` bucket into this holder.
//!
//! What stays outside: the `Arc<Mutex<…>>` ingest queue and recorder slot
//! themselves (they are shared with the reader-thread hook at attach time
//! and swapping them would break the hook), and the native channel (the
//! semantic domain's side channel). This holder owns the STREAM and its
//! bookkeeping: what was absorbed, up to where, under which anchor.
//!
//! Pure state — no backend, no locks (the hook-facing queues are handed
//! in pre-built and only cloned out).

use crate::events::TerminalEventQueue;

/// The session's event stream + absorption watermarks for one session.
pub(crate) struct SessionEventState {
    /// Per-session terminal event queue (Wave B item 12): every observe,
    /// send, resize, and process-state change appends here. Waits, audits,
    /// incremental observation, and run persistence all read this one
    /// stream.
    queue: TerminalEventQueue,
    /// Per-consumer observation cursors (Wave B item 13): named consumers
    /// (`hermes`, `audit`, `explorer`, `recording`, ...) each remember their
    /// own position in the event stream. Cursors live in the session so a
    /// reconnecting consumer resumes where it left off, but reading is
    /// stateless — `events_since` never mutates a cursor implicitly.
    cursors: std::collections::HashMap<String, u64>,
    /// Monotonic anchor sequence (re-review P1: `ObservationAnchor.index`
    /// must be a real per-session counter, not a hardcoded 0). Allocated by
    /// the session's `next_anchor`; shared by every executor path.
    next_anchor_seq: u64,
    /// Item 22: the answered counter last folded into the event queue —
    /// dedup anchor for query-answer absorption.
    last_query_answered_seq: u64,
    /// How many native-channel events have already been folded into the
    /// event queue (re-review P1: mode-independent ingestion). Tracked by
    /// the channel's monotonic native_seq, NOT a Vec index: once the
    /// channel's ring hits its cap, len stays constant while old events
    /// evict, and a positional cursor would permanently believe there is
    /// nothing new (re-review P0 absorption stall).
    native_events_absorbed_seq: u64,
}

impl SessionEventState {
    pub(crate) fn new() -> Self {
        SessionEventState {
            queue: TerminalEventQueue::new(),
            cursors: std::collections::HashMap::new(),
            next_anchor_seq: 0,
            last_query_answered_seq: 0,
            native_events_absorbed_seq: 0,
        }
    }

    /// The event queue (read paths: since/all/evicted/total/...).
    pub(crate) fn queue(&self) -> &TerminalEventQueue {
        &self.queue
    }

    /// The event queue (push / drain).
    pub(crate) fn queue_mut(&mut self) -> &mut TerminalEventQueue {
        &mut self.queue
    }

    /// A named consumer's stored cursor (0 when the consumer never read).
    pub(crate) fn cursor(&self, consumer: &str) -> u64 {
        self.cursors.get(consumer).copied().unwrap_or(0)
    }

    /// Persist a consumer's cursor after a batch read.
    pub(crate) fn set_cursor(&mut self, consumer: &str, seq: u64) {
        self.cursors.insert(consumer.to_string(), seq);
    }

    /// Allocate the next observation anchor index.
    pub(crate) fn next_anchor(&mut self) -> u64 {
        let n = self.next_anchor_seq;
        self.next_anchor_seq += 1;
        n
    }

    /// Item 22: the answered counter already folded into the stream.
    pub(crate) fn last_query_answered_seq(&self) -> u64 {
        self.last_query_answered_seq
    }

    pub(crate) fn set_query_answered_seq(&mut self, seq: u64) {
        self.last_query_answered_seq = seq;
    }

    /// The native-channel absorption watermark.
    pub(crate) fn native_absorbed_seq(&self) -> u64 {
        self.native_events_absorbed_seq
    }

    pub(crate) fn set_native_absorbed_seq(&mut self, seq: u64) {
        self.native_events_absorbed_seq = seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TerminalEventKind;

    #[test]
    fn anchors_are_monotonic_per_session() {
        let mut st = SessionEventState::new();
        assert_eq!(st.next_anchor(), 0);
        assert_eq!(st.next_anchor(), 1);
        assert_eq!(st.next_anchor(), 2);
    }

    #[test]
    fn cursors_default_to_zero_and_persist_per_consumer() {
        let mut st = SessionEventState::new();
        assert_eq!(st.cursor("hermes"), 0);
        st.set_cursor("hermes", 7);
        st.set_cursor("audit", 3);
        assert_eq!(st.cursor("hermes"), 7);
        assert_eq!(st.cursor("audit"), 3);
        assert_eq!(st.cursor("explorer"), 0);
    }

    #[test]
    fn queue_pushes_are_visible_through_the_holder() {
        let mut st = SessionEventState::new();
        st.queue_mut().push("sess-1", 0, TerminalEventKind::Bell);
        assert_eq!(st.queue().total(), 1);
        assert_eq!(st.queue().last_seq(), 1);
        assert_eq!(st.queue().evicted(), 0);
    }

    #[test]
    fn absorption_watermarks_start_zero_and_stick() {
        let mut st = SessionEventState::new();
        assert_eq!(st.last_query_answered_seq(), 0);
        assert_eq!(st.native_absorbed_seq(), 0);
        st.set_query_answered_seq(4);
        st.set_native_absorbed_seq(9);
        assert_eq!(st.last_query_answered_seq(), 4);
        assert_eq!(st.native_absorbed_seq(), 9);
    }
}
