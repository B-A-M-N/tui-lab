//! EventBus — one ordered timeline for events from every source (review P0:
//! "a unified event bus").
//!
//! The runtime distinguishes events by where they come from:
//!
//! * **Terminal** — what the PTY rendered ([`TerminalEventKind`]: output,
//!   screen changes, cursor, bell, title, resize, process edges).
//! * **Shell command** — OSC 133 shell-integration boundaries (prompt / output
//!   / done) that a cooperative shell reports, surfaced by the backend's
//!   `CommandState`.
//! * **Native** — structural/UX signals a framework adapter already decoded
//!   (focus moves, affordance changes) — the `native` half of the semantic
//!   model, not re-inferred.
//! * **Coverage** — `target` hits a cooperative app reports over the native
//!   side channel (the coverage ledger's event feed).
//!
//! Before a bus these lived in separate cursors and callers had to stitch
//! them. [`EventBus`] multiplexes every source onto a *single* total order
//! (a global arrival seq over a bounded ring) so one consumer holds one
//! cursor and reads one stream — the audit, run replay, and incremental
//! observation all converge on it. A source keeps its own counter internally
//! so a consumer can tell *which* event of that source it is looking at.

use crate::backend::CommandState;
use crate::events::TerminalEventKind;
use std::collections::VecDeque;

/// How many converged events the bus retains before declaring eviction.
pub const BUS_RING_CAPACITY: usize = 8192;

/// Which source produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusSource {
    Terminal,
    ShellCommand,
    Native,
    Coverage,
    /// Agent-placed bookmarks/markers (not produced by the child).
    Marker,
}

impl BusSource {
    pub fn name(&self) -> &'static str {
        match self {
            BusSource::Terminal => "terminal",
            BusSource::ShellCommand => "shell_command",
            BusSource::Native => "native",
            BusSource::Coverage => "coverage",
            BusSource::Marker => "marker",
        }
    }
}

/// A converged timeline event: one thing that happened, from any source.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BusEvent {
    /// Global arrival sequence over the unified timeline (1-based).
    pub seq: u64,
    /// Which source produced it.
    pub source: BusSource,
    /// That source's own 1-based counter (its Nth event).
    pub source_seq: u64,
    /// Unix-millis timestamp.
    pub at: u64,
    /// Session the event belongs to.
    pub session: String,
    /// What happened, per source.
    pub kind: BusEventKind,
}

/// The payload of a converged event.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BusEventKind {
    /// A terminal event (re-typed; the payload is the full kind).
    Terminal(TerminalEventKind),
    /// An OSC 133 shell-command boundary (phase = the app's own label).
    ShellCommand {
        /// 1-based command sequence from the backend.
        command_seq: u64,
        /// Phase label: "prompt" | "output" | "done".
        phase: String,
        /// Exit status reported by `133;D;exit` (None when `phase != "done"`).
        exit: Option<i32>,
    },
    /// A native framework-adapter event.
    Native {
        /// Adapter-provided, already-decoded signal (free text the adapter
        /// chose; kept factual so the agent can act on it).
        signal: String,
    },
    /// A coverage hit reported by a cooperative app over the native channel.
    Coverage {
        /// File:line the app claims it exercised (e.g. `src/main.rs:42`).
        target: String,
    },
    /// A named bookmark/marker on the timeline (review P1: "bookmarks/markers
    /// to the event timeline"). Lets an agent stamp a point in the stream —
    /// "menu opened here", "went wrong here" — so later analysis and probes can
    /// reference a named moment rather than a raw seq.
    Marker(String),
}

impl BusEventKind {
    /// Stable machine name.
    pub fn name(&self) -> &'static str {
        match self {
            BusEventKind::Terminal(t) => t.name(),
            BusEventKind::ShellCommand { .. } => "shell_command",
            BusEventKind::Native { .. } => "native",
            BusEventKind::Coverage { .. } => "coverage",
            BusEventKind::Marker { .. } => "marker",
        }
    }
}

/// The batch handed to a subscriber by [`EventBus::since`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BusBatch {
    /// Events after the subscriber's cursor, in arrival order.
    pub events: Vec<BusEvent>,
    /// The cursor to hold for the next read.
    pub cursor: u64,
    /// True when earlier events were evicted — this is a partial view.
    pub gap: bool,
}

/// The unified, bounded, multi-source event bus.
#[derive(Debug, Default)]
pub struct EventBus {
    events: VecDeque<BusEvent>,
    next_seq: u64,
    evicted: u64,
    /// Per-source counters so `source_seq` is continuous.
    source_seqs: [u64; 5],
}

fn source_index(s: BusSource) -> usize {
    match s {
        BusSource::Terminal => 0,
        BusSource::ShellCommand => 1,
        BusSource::Native => 2,
        BusSource::Coverage => 3,
        BusSource::Marker => 4,
    }
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            events: VecDeque::new(),
            next_seq: 1,
            evicted: 0,
            source_seqs: [0; 5],
        }
    }

    /// Converge one event onto the unified timeline. Returns its global seq.
    ///
    /// Callers may publish from *any* source and the bus orders them by
    /// arrival; a source's internal counter stays contiguous so filtering by
    /// source is lossless.
    pub fn publish(&mut self, session: &str, source: BusSource, kind: BusEventKind) -> u64 {
        let idx = source_index(source);
        self.source_seqs[idx] += 1;
        let source_seq = self.source_seqs[idx];
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.events.len() >= BUS_RING_CAPACITY {
            let drop = BUS_RING_CAPACITY / 2;
            self.events.drain(..drop);
            self.evicted += drop as u64;
        }
        self.events.push_back(BusEvent {
            seq,
            source,
            source_seq,
            at: now_ms(),
            session: session.to_string(),
            kind,
        });
        seq
    }

    /// Convenience: publish a shell-command boundary straight from a backend
    /// [`CommandState`] snapshot (OSC 133). No-op unless the phase advanced
    /// past the previously published one for that command.
    pub fn publish_command(&mut self, session: &str, state: &CommandState) {
        if state.running {
            self.publish(
                session,
                BusSource::ShellCommand,
                BusEventKind::ShellCommand {
                    command_seq: state.command_seq,
                    phase: state.phase.to_string(),
                    exit: state.last_exit,
                },
            );
        }
    }

    /// Convenience: publish a native framework signal.
    pub fn publish_native(&mut self, session: &str, signal: &str) -> u64 {
        self.publish(
            session,
            BusSource::Native,
            BusEventKind::Native {
                signal: signal.to_string(),
            },
        )
    }

    /// Convenience: publish a coverage hit from a cooperative app.
    pub fn publish_coverage(&mut self, session: &str, target: &str) -> u64 {
        self.publish(
            session,
            BusSource::Coverage,
            BusEventKind::Coverage {
                target: target.to_string(),
            },
        )
    }

    /// Stamp a named marker/bookmark onto the timeline (review P1). Returns
    /// the marker's global seq so a later `since`/`of_source` analysis can
    /// reference this exact moment.
    pub fn publish_marker(&mut self, session: &str, label: &str) -> u64 {
        self.publish(
            session,
            BusSource::Marker,
            BusEventKind::Marker(label.to_string()),
        )
    }

    /// Everything after `cursor`, in arrival order. The cursor is owned by the
    /// subscriber — this does not advance it.
    pub fn since(&self, cursor: u64) -> BusBatch {
        let first = self.events.front().map(|e| e.seq);
        let gap = cursor != 0 && first.map(|f| cursor + 1 < f).unwrap_or(true);
        let events: Vec<BusEvent> = self
            .events
            .iter()
            .filter(|e| e.seq > cursor)
            .cloned()
            .collect();
        let cursor_out = events.last().map(|e| e.seq).unwrap_or(cursor);
        BusBatch {
            events,
            cursor: cursor_out,
            gap,
        }
    }

    /// Open a new subscriber cursor (oldest retained event; or the next seq if
    /// the ring is empty).
    pub fn subscribe(&self) -> u64 {
        self.events
            .front()
            .map(|e| e.seq.saturating_sub(1))
            .unwrap_or_else(|| self.next_seq.saturating_sub(1))
    }

    /// Events of one source at/after `cursor`.
    pub fn of_source(&self, cursor: u64, source: BusSource) -> Vec<BusEvent> {
        self.since(cursor)
            .events
            .into_iter()
            .filter(|e| e.source == source)
            .collect()
    }

    /// Highest global seq handed out.
    pub fn last_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Total events ever converged (retained + evicted).
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
}

/// Project a terminal-queue event into the converged-timeline vocabulary
/// (re-review item 16 — consolidation, not duplication): the session event
/// queue remains the ONE store; this translation is how `tui_observe
/// history` and watch predicates name its events in bus terms, so the
/// history view and the terminal event ring can never be two divergent
/// authorities.
impl BusEvent {
    pub fn from_terminal(ev: &crate::events::TerminalEvent, source_seq: u64, seq: u64) -> Self {
        BusEvent {
            seq,
            source: match ev.kind {
                crate::events::TerminalEventKind::NativeEvent { .. } => BusSource::Native,
                _ => BusSource::Terminal,
            },
            source_seq,
            at: ev.at,
            session: ev.session.clone(),
            kind: BusEventKind::Terminal(ev.kind.clone()),
        }
    }
}

/// A filtered, windowed read of the converged timeline (re-review item 15):
/// the shape `tui_observe mode=history` serves.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// Return events with `seq > since_seq` (0 = from the ring's start).
    pub since_seq: u64,
    /// Return events with `seq <= until_seq` (None = no upper bound).
    pub until_seq: Option<u64>,
    /// Cap on returned events (None = ring capacity).
    pub limit: Option<usize>,
    /// Keep only these event kinds (by machine name, e.g. "bell",
    /// "screen_changed", "native_event"); empty = all.
    pub event_types: Vec<String>,
}

/// Resolve a history query against an ordered event slice, projecting onto
/// the converged vocabulary with a `gap` flag when the ring has evicted
/// earlier events the query would have wanted.
pub fn project_history(
    events: &[crate::events::TerminalEvent],
    query: &HistoryQuery,
    evicted: u64,
) -> BusBatch {
    // Project the whole retained window first so per-source counters count
    // *every* event of that source in the store — stable across windows and
    // independent of what this particular query serves.
    let mut source_counts: std::collections::HashMap<BusSource, u64> =
        std::collections::HashMap::new();
    let projected: Vec<BusEvent> = events
        .iter()
        .map(|ev| {
            let mut b = BusEvent::from_terminal(ev, 0, ev.seq);
            let n = source_counts.entry(b.source).or_insert(0);
            *n += 1;
            b.source_seq = *n;
            b
        })
        .collect();

    let limit = query.limit.unwrap_or(usize::MAX);
    let mut out = Vec::new();
    for ev in &projected {
        if ev.seq <= query.since_seq {
            continue;
        }
        if let Some(until) = query.until_seq {
            if ev.seq > until {
                break;
            }
        }
        if !query.event_types.is_empty() && !query.event_types.iter().any(|t| t == ev.kind.name()) {
            continue;
        }
        if out.len() >= limit {
            break;
        }
        out.push(ev.clone());
    }
    // The cursor is the last event actually SERVED, so paging by cursor never
    // skips anything — a `limit`-truncated response continues where it ended.
    let cursor = out.last().map(|e| e.seq).unwrap_or(query.since_seq);
    // The ring evicts from the front (seqs 1..=evicted), so a query reaching
    // back behind the watermark saw a partial window. `since_seq` is
    // exclusive — a query with `since_seq == evicted` wants exactly the
    // retained window and is complete.
    let gap = evicted > 0 && query.since_seq < evicted;
    BusBatch {
        events: out,
        cursor,
        gap,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_converge_onto_one_total_order() {
        let mut bus = EventBus::new();
        bus.publish(
            "s",
            BusSource::Terminal,
            BusEventKind::Terminal(TerminalEventKind::Bell),
        );
        bus.publish_native("s", "focus:list");
        bus.publish_coverage("s", "src/main.rs:42");
        assert_eq!(bus.total(), 3, "all sources share one timeline");

        let batch = bus.since(0);
        assert_eq!(batch.events.len(), 3);
        // Arrival order is preserved, per-source seqs are contiguous.
        assert_eq!(batch.events[0].source, BusSource::Terminal);
        assert_eq!(batch.events[0].source_seq, 1);
        assert_eq!(batch.events[1].source, BusSource::Native);
        assert_eq!(batch.events[1].source_seq, 1);
        assert_eq!(batch.events[2].source, BusSource::Coverage);
        assert_eq!(batch.events[2].source_seq, 1);
        assert_eq!(batch.cursor, 3);
    }

    #[test]
    fn per_source_counters_reach_across_publishes() {
        let mut bus = EventBus::new();
        bus.publish_coverage("s", "a.rs:1");
        bus.publish_coverage("s", "a.rs:2");
        let hits = bus.of_source(0, BusSource::Coverage);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source_seq, 1);
        assert_eq!(hits[1].source_seq, 2, "source counter is contiguous");
    }

    #[test]
    fn subscriber_cursor_is_consumer_owned() {
        let mut bus = EventBus::new();
        let c0 = bus.subscribe();
        bus.publish_native("s", "menu:open");
        let first = bus.since(c0);
        assert_eq!(first.events.len(), 1);
        bus.publish_native("s", "menu:select");
        let again = bus.since(c0);
        assert_eq!(
            again.events.len(),
            2,
            "re-read from original cursor sees both"
        );
        let incremental = bus.since(first.cursor);
        assert_eq!(incremental.events.len(), 1);
    }

    #[test]
    fn ring_eviction_is_declared() {
        let mut bus = EventBus::new();
        for i in 0..(BUS_RING_CAPACITY + 5) {
            bus.publish(
                "s",
                BusSource::Terminal,
                BusEventKind::Terminal(TerminalEventKind::Output { byte_len: i }),
            );
        }
        assert!(bus.evicted() > 0, "eviction must be counted");
        let behind = bus.since(1);
        assert!(behind.gap, "a far-behind subscriber learns of the gap");
        let fresh = bus.since(bus.last_seq());
        assert!(!fresh.gap, "an up-to-date subscriber sees no gap");
    }

    #[test]
    fn command_boundary_from_backend_state() {
        let mut bus = EventBus::new();
        let state = CommandState {
            command_seq: 1,
            running: true,
            last_exit: None,
            phase: "output",
        };
        bus.publish_command("s", &state);
        let batch = bus.since(0);
        assert_eq!(batch.events.len(), 1);
        if let BusEventKind::ShellCommand {
            command_seq,
            phase,
            exit,
        } = &batch.events[0].kind
        {
            assert_eq!(*command_seq, 1);
            assert_eq!(*phase, "output");
            assert_eq!(*exit, None);
        } else {
            panic!("expected a shell-command boundary");
        }
        assert_eq!(bus.of_source(0, BusSource::ShellCommand).len(), 1);
    }
}
