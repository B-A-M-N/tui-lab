//! Event queue surface: per-consumer cursors, history, drain, stats.
//!
//! Impl-family extraction (Phase 2): the `Session` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. `Session` remains the single
//! per-session concurrency authority — no independent locking is
//! introduced. Signatures, visibility, and callers are unchanged.

use super::*;

impl Session {
    /// Public absorb for latency measurement (re-review item 26): the
    /// executor folds pending reader-thread byte facts before reading the
    /// event queue, so input→first-byte is computed on complete data.
    pub fn absorb_ingest_now(&mut self) {
        self.absorb_pending_ingest();
        self.absorb_query_answers();
    }

    /// Read events after `cursor` WITHOUT moving it (per-consumer cursors,
    /// Wave B item 13: the caller owns the position).
    pub fn events_since(&self, cursor: u64) -> crate::events::EventBatch {
        self.events.since(cursor)
    }

    /// Every retained event, in seq order (re-review item 15: history reads
    /// the whole retained window and filters by query, not by cursor).
    pub fn all_events(&self) -> Vec<crate::events::TerminalEvent> {
        self.events.all()
    }

    /// Events evicted by the event ring (the honest partial-window flag).
    pub fn events_evicted(&self) -> u64 {
        self.events.evicted()
    }

    /// Read events after the named consumer's stored cursor, then advance
    /// that cursor to the served position. Unknown consumers start at 0.
    pub fn events_for_consumer(&mut self, consumer: &str) -> crate::events::EventBatch {
        let cursor = self.cursors.get(consumer).copied().unwrap_or(0);
        let batch = self.events.since(cursor);
        self.cursors.insert(consumer.to_string(), batch.cursor);
        batch
    }

    /// The session's whole retained event stream, plus declared-gap stats.
    pub fn event_queue_stats(&self) -> serde_json::Value {
        serde_json::json!({
            "total": self.events.total(),
            "retained": self.events.retained(),
            "evicted": self.events.evicted(),
            "last_seq": self.events.last_seq(),
        })
    }

    /// Drain retained events for run persistence (keeps seq continuity).
    pub fn drain_events(&mut self) -> Vec<crate::events::TerminalEvent> {
        self.events.drain()
    }

    /// Current event sequence state (for action-anchored waits).
    pub fn event_state(&self) -> TerminalEventState {
        self.backend.event_state()
    }

    /// The session event queue's last sequence number (highest seq assigned
    /// so far, 0 when empty). Used to anchor event-based completion so an
    /// event firing immediately after the send is never missed.
    pub fn event_queue_last_seq(&self) -> u64 {
        self.events.last_seq()
    }
}

#[cfg(test)]
mod tests {
    // The tests live on the emission path: `emit_frame_events` is private
    // to `state.rs`, so they run there.
}
