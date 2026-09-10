//! Terminal event queue (re-review Wave B item 12) — the one consumable
//! event stream for a session.
//!
//! Before this module the runtime only had *counters* (`screen_seq`,
//! `output_seq`, `bell_seq`, `title_seq`): code could ask "did something
//! happen since N?" but never "what happened?". Waits, audit evidence, run
//! replay, and incremental observation all want the same answer, so they all
//! consume this one stream.
//!
//! It also hosts the bounded multi-source [`bus::EventHistoryProjection`]
//! that converges terminal, shell-command (OSC 133), native, and coverage
//! snapshots onto one ordered view. The queue below is the canonical
//! terminal event authority; the projection is not another authority.
//!
//! Design constraints:
//! - **Bounded**: a ring of [`EVENT_RING_CAPACITY`] events; eviction is
//!   declared with `first_seq` so a consumer that falls far behind learns
//!   it dropped events rather than silently skipping.
//! - **Append-only**: events are never mutated once pushed.
//! - **Cursor-friendly**: consumers hold a plain `u64` seq; reading
//!   `since(seq)` yields everything after it and advances nothing — cursors
//!   are owned by the consumer (per-consumer cursors, item 13), not by the
//!   queue.

/// Upper bound on retained events per session. Sized so a full exploration
/// step burst fits comfortably; eviction is declared, not silent.
pub const EVENT_RING_CAPACITY: usize = 4096;

pub mod bus;

pub use bus::{
    project_history, BusBatch, BusEvent, BusEventKind, BusSource, EventHistoryProjection,
    HistoryQuery,
};

/// One thing that happened on a terminal, in order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalEvent {
    /// Monotonic sequence (per session, 1-based; 0 is "no events yet").
    pub seq: u64,
    /// Unix-millis timestamp (human correlation only).
    pub at: u64,
    /// Monotonic milliseconds since process start. Authoritative for
    /// causal ordering and durations; immune to wall-clock changes.
    #[serde(default)]
    pub monotonic_ms: u64,
    /// Session the event belongs to (denormalized so a drained ring can be
    /// shipped to run artifacts without extra context).
    pub session: String,
    /// Session generation at event time.
    pub generation: u32,
    /// What happened.
    pub kind: TerminalEventKind,
}

/// The event vocabulary (re-review item 12). Kept small and factual:
/// everything here is derivable from data the backend already produces.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalEventKind {
    /// Bytes arrived from the PTY.
    Output {
        /// Byte count (never the bytes themselves — payloads can contain
        /// secrets; counts are safe and useful for replay forensics).
        byte_len: usize,
    },
    /// The parsed screen content changed (structure-level).
    ScreenChanged {
        /// Rows with any changed cell (empty when unknown).
        dirty_rows: Vec<u16>,
    },
    /// Rendered appearance changed (style/attribute level).
    VisualChanged,
    /// Cursor position or visibility changed.
    CursorMoved { x: u16, y: u16 },
    /// The terminal bell rang.
    Bell,
    /// The terminal title changed.
    TitleChanged { title: String },
    /// The viewport was resized.
    Resize { cols: u16, rows: u16 },
    /// A semantic focus change was inferred (from observation analysis).
    FocusChanged {
        from: Option<String>,
        to: Option<String>,
    },
    /// The child process started (generation N).
    ProcessStarted,
    /// The child process exited.
    ProcessExited {
        exit_code: Option<i32>,
        exit_signal: Option<String>,
    },
    /// Semantic analysis output changed (controls/regions/affordances).
    SemanticChanged,
    /// Re-review P1 (unified event substrate): an event from the app's
    /// native semantic side-channel. These are the app's OWN words about
    /// itself, so they carry the strongest available evidence.
    NativeEvent {
        /// `focus`, `activate`, `coverage`, or any app-declared verb.
        event: String,
        /// The node id / coverage target the event names.
        target: String,
    },
    /// Item 22: the engine's device-query responder answered a query the
    /// app sent — measured evidence of a real query/response round trip.
    /// Emitted when the answer bytes are written back to the PTY.
    QueryAnswered {
        /// Query class: `da1`, `da2`, `da3`, `dsr_cpr`, `dsr_status`,
        /// `decrqm`, `kitty_flags`, `osc_color`.
        class: String,
    },
}

impl TerminalEventKind {
    /// Stable machine name (ledger/evidence friendly).
    pub fn name(&self) -> &'static str {
        match self {
            TerminalEventKind::Output { .. } => "output",
            TerminalEventKind::ScreenChanged { .. } => "screen_changed",
            TerminalEventKind::VisualChanged => "visual_changed",
            TerminalEventKind::CursorMoved { .. } => "cursor_moved",
            TerminalEventKind::Bell => "bell",
            TerminalEventKind::TitleChanged { .. } => "title_changed",
            TerminalEventKind::Resize { .. } => "resize",
            TerminalEventKind::FocusChanged { .. } => "focus_changed",
            TerminalEventKind::ProcessStarted => "process_started",
            TerminalEventKind::ProcessExited { .. } => "process_exited",
            TerminalEventKind::SemanticChanged => "semantic_changed",
            TerminalEventKind::NativeEvent { .. } => "native_event",
            TerminalEventKind::QueryAnswered { .. } => "query_answered",
        }
    }
}

/// The event snapshot handed to a consumer by [`TerminalEventQueue::since`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventBatch {
    /// The events after the consumer's cursor, in seq order.
    pub events: Vec<TerminalEvent>,
    /// The cursor to hold for the next read (last seq served; `since` of
    /// this returns an empty batch when nothing new happened).
    pub cursor: u64,
    /// True when events between the consumer's cursor and the oldest
    /// retained event were evicted — the batch is a *partial* view.
    pub gap: bool,
    /// Oldest retained seq (meaningful only when `gap` is true).
    pub first_available: Option<u64>,
}

impl EventBatch {
    /// An empty batch at cursor `cursor`.
    pub fn empty(cursor: u64) -> Self {
        EventBatch {
            events: Vec::new(),
            cursor,
            gap: false,
            first_available: None,
        }
    }
}

/// Per-session bounded event queue.
#[derive(Debug, Default)]
pub struct TerminalEventQueue {
    events: Vec<TerminalEvent>,
    next_seq: u64,
    /// Count of evicted events (declared eviction, like the run ledger).
    evicted: u64,
}

impl TerminalEventQueue {
    pub fn new() -> Self {
        TerminalEventQueue {
            events: Vec::new(),
            next_seq: 1,
            evicted: 0,
        }
    }

    /// Append one event; assigns seq and timestamp. Returns the event's seq.
    pub fn push(&mut self, session: &str, generation: u32, kind: TerminalEventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let ev = TerminalEvent {
            seq,
            at: now_ms(),
            monotonic_ms: monotonic_ms(),
            session: session.to_string(),
            generation,
            kind,
        };
        if self.events.len() >= EVENT_RING_CAPACITY {
            let drop = EVENT_RING_CAPACITY / 2;
            self.events.drain(..drop);
            self.evicted += drop as u64;
        }
        self.events.push(ev);
        seq
    }

    /// Everything after `cursor` (events with `seq > cursor`). The cursor is
    /// owned by the caller — nothing here advances it.
    pub fn since(&self, cursor: u64) -> EventBatch {
        if self.events.is_empty() {
            let mut batch = EventBatch::empty(self.last_seq());
            batch.gap = self.evicted > 0 && cursor < self.first_available().unwrap_or(u64::MAX);
            batch.first_available = self.first_available();
            return batch;
        }
        let first_available = self.events[0].seq;
        // Cursor 0 explicitly means "from the beginning": when the retained
        // window begins later because of eviction, that beginning is itself
        // a gap, not a complete history.
        let gap = cursor + 1 < first_available;
        let events: Vec<TerminalEvent> = self
            .events
            .iter()
            .filter(|e| e.seq > cursor)
            .cloned()
            .collect();
        let cursor_out = events.last().map(|e| e.seq).unwrap_or(cursor);
        EventBatch {
            events,
            cursor: cursor_out,
            gap,
            first_available: gap.then_some(first_available),
        }
    }

    /// Every retained event, in seq order (re-review item 15: the history
    /// projection reads the whole retained window, then filters — `since`
    /// is cursor-shaped, history is query-shaped).
    pub fn all(&self) -> Vec<TerminalEvent> {
        self.events.clone()
    }

    /// Highest seq handed out so far (0 when empty).
    pub fn last_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Oldest retained seq, if any events are held.
    pub fn first_available(&self) -> Option<u64> {
        self.events.first().map(|e| e.seq)
    }

    /// Total events ever pushed (retained + evicted).
    pub fn total(&self) -> u64 {
        self.next_seq - 1
    }

    /// Events currently retained.
    pub fn retained(&self) -> usize {
        self.events.len()
    }

    /// Events evicted by the ring bound.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Drain all retained events (run persistence / artifact export). The
    /// queue keeps its seq continuity; the caller owns the copies.
    pub fn drain(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.events)
    }
}

fn now_ms() -> u64 {
    unix_ms()
}

/// Milliseconds from a monotonic process-start origin.
pub fn monotonic_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Unix-millis timestamp (public: transaction latency math shares the event
/// queue's clock so input→first-byte is measured on ONE timeline).
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> TerminalEventQueue {
        TerminalEventQueue::new()
    }

    #[test]
    fn events_are_ordered_and_timestamped() {
        let mut q = q();
        let a = q.push("s", 1, TerminalEventKind::Bell);
        let b = q.push("s", 1, TerminalEventKind::ProcessStarted);
        assert!(b > a, "seq must increase");
        let batch = q.since(0);
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.cursor, b);
        assert!(batch.events[0].at > 0, "timestamped");
        assert!(
            batch.events[0].monotonic_ms <= batch.events[1].monotonic_ms,
            "monotonic stamps are causal and non-decreasing"
        );
        assert_eq!(batch.events[1].kind.name(), "process_started");
    }

    /// The consumer-owned cursor contract: reading twice from the same
    /// cursor must not lose events, and reading from the served cursor
    /// yields nothing new.
    #[test]
    fn cursor_is_consumer_owned() {
        let mut q = q();
        q.push("s", 1, TerminalEventKind::Bell);
        let first = q.since(0);
        assert_eq!(first.events.len(), 1);
        // New event after the read.
        q.push("s", 1, TerminalEventKind::Bell);
        // Re-read from the ORIGINAL cursor: still see both.
        let again = q.since(0);
        assert_eq!(again.events.len(), 2);
        // Read from the served cursor: only the new one.
        let incremental = q.since(first.cursor);
        assert_eq!(incremental.events.len(), 1);
        // And then nothing.
        assert!(q.since(incremental.cursor).events.is_empty());
    }

    #[test]
    fn ring_eviction_is_declared() {
        let mut q = q();
        for i in 0..(EVENT_RING_CAPACITY + 10) {
            q.push("s", 1, TerminalEventKind::Output { byte_len: i });
        }
        assert_eq!(q.total(), EVENT_RING_CAPACITY as u64 + 10);
        assert_eq!(
            q.retained(),
            EVENT_RING_CAPACITY - EVENT_RING_CAPACITY / 2 + 10
        );
        assert!(q.evicted() > 0, "eviction must be counted");
        // A far-behind consumer learns about the gap.
        let batch = q.since(1);
        assert!(batch.gap, "consumer behind eviction must see the gap");
        let from_zero = q.since(0);
        assert!(
            from_zero.gap,
            "since(0) means 'from the beginning'; eviction makes that a partial view"
        );
        assert_eq!(from_zero.first_available, q.first_available());
        assert_eq!(batch.first_available, q.first_available());
        // An up-to-date consumer sees no gap.
        let fresh = q.since(q.last_seq());
        assert!(!fresh.gap);
    }

    #[test]
    fn drain_leaves_seq_continuity() {
        let mut q = q();
        q.push("s", 1, TerminalEventKind::Bell);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert!(q.since(0).events.is_empty());
        let seq = q.push("s", 1, TerminalEventKind::Bell);
        assert!(seq > drained[0].seq, "seq must not restart after drain");
    }

    /// The JSON shape is stable — this is what lands in run artifacts.
    #[test]
    fn event_json_roundtrip() {
        let ev = TerminalEvent {
            seq: 7,
            at: 1234,
            monotonic_ms: 42,
            session: "s1".into(),
            generation: 2,
            kind: TerminalEventKind::ScreenChanged {
                dirty_rows: vec![3, 8],
            },
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(json.contains("\"type\":\"screen_changed\""));
        assert!(json.contains("\"dirty_rows\":[3,8]"));
        let back: TerminalEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ev);
    }
}
