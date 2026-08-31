//! Watch — a connective primitive for "wait until <condition> or timeout"
//! (review P1: "Add a `Watch` connective primitive").
//!
//! Distinct from a single `observe`/`probe`: a watch is a *standing* predicate
//! over the event timeline. It opens a cursor on the unified
//! [`EventBus`](crate::events::bus::EventBus), then drives forward until the
//! declared condition is met, a host marker is hit, or the budget expires —
//! returning an honest outcome (`Met`/`MatchedMarker`/`Timeout`), never a
//! silently-assumed `settled`. This is how an agent expresses "keep going until
//! the menu appears" without blocking on stability and without guessing.

use std::time::{Duration, Instant};

use crate::events::bus::{BusEvent, BusEventKind, BusSource, EventBus};

/// What a watch is looking for, evaluated against each new converged event.
#[derive(Debug, Clone)]
pub enum WatchCondition {
    /// Fires the first time a marker with this exact label is seen.
    MarkerSeen(String),
    /// Fires the first time any event from this source arrives.
    SourceActivity(BusSource),
    /// Fires the first time any converged event arrives at/after the cursor.
    AnyActivity,
}

/// Why a watch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    /// A marker with the target label was observed.
    MatchedMarker {
        /// The marker's global seq.
        at_seq: u64,
    },
    /// The source-activity condition was met.
    MatchedSource {
        source: BusSource,
        at_seq: u64,
    },
    /// Any activity was observed.
    MatchedAny {
        at_seq: u64,
    },
    /// No event satisfied the condition before the budget elapsed.
    Timeout,
}

/// A standing watch over the event bus.
#[derive(Debug, Clone)]
pub struct WatchSpec {
    /// What to wait for.
    pub condition: WatchCondition,
    /// Overall ceiling; the watch returns `Timeout` at this point regardless
    /// of anything arriving.
    pub budget: Duration,
    /// Poll interval for a live bus (how often `since` is asked).
    pub poll: Duration,
}

impl WatchSpec {
    /// Run the watch against a live bus until the condition is met or the
    /// budget elapses. Returns the [`WatchOutcome`].
    ///
    /// `cursor` is the starting point (e.g. from [`EventBus::subscribe`]).
    pub fn run(&self, bus: &EventBus, cursor: u64) -> WatchOutcome {
        let start = Instant::now();
        let mut cursor = cursor;
        loop {
            let batch = bus.since(cursor);
            for ev in &batch.events {
                if let Some(outcome) = self.matches(ev) {
                    return outcome;
                }
                cursor = ev.seq;
            }
            if start.elapsed() >= self.budget {
                return WatchOutcome::Timeout;
            }
            std::thread::sleep(self.poll);
        }
    }

    /// Single-event predicate.
    fn matches(&self, ev: &BusEvent) -> Option<WatchOutcome> {
        match &self.condition {
            WatchCondition::MarkerSeen(label) => {
                if let BusEventKind::Marker(m) = &ev.kind {
                    if m == label {
                        return Some(WatchOutcome::MatchedMarker { at_seq: ev.seq });
                    }
                }
                None
            }
            WatchCondition::SourceActivity(src) => {
                if ev.source == *src {
                    Some(WatchOutcome::MatchedSource {
                        source: ev.source,
                        at_seq: ev.seq,
                    })
                } else {
                    None
                }
            }
            WatchCondition::AnyActivity => Some(WatchOutcome::MatchedAny { at_seq: ev.seq }),
        }
    }
}

/// A convenience builder so callers can name a marker watch tersely.
pub fn until_marker(label: &str, budget: Duration) -> WatchSpec {
    WatchSpec {
        condition: WatchCondition::MarkerSeen(label.to_string()),
        budget,
        poll: Duration::from_millis(15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::{BusEventKind, EventBus};

    #[test]
    fn watch_fires_on_marker() {
        let mut bus = EventBus::new();
        bus.publish_marker("s", "phase1:start");
        let cursor = bus.subscribe();
        bus.publish_marker("s", "menu:opened");
        let outcome = until_marker("menu:opened", Duration::from_secs(1)).run(&bus, cursor);
        assert_eq!(
            outcome,
            WatchOutcome::MatchedMarker {
                at_seq: bus.last_seq()
            }
        );
    }

    #[test]
    fn watch_times_out_honestly() {
        let bus = EventBus::new();
        let cursor = bus.subscribe();
        let outcome = until_marker("never", Duration::from_millis(50)).run(&bus, cursor);
        assert_eq!(outcome, WatchOutcome::Timeout, "empty bus must time out, not lie");
    }

    #[test]
    fn watch_does_not_fire_on_a_different_marker() {
        let mut bus = EventBus::new();
        bus.publish_marker("s", "other");
        let cursor = bus.subscribe();
        bus.publish_marker("s", "target");
        // A marker labeled 'other' before the cursor is not matched; the
        // target marker is.
        let outcome = until_marker("target", Duration::from_secs(1)).run(&bus, cursor);
        assert_eq!(outcome, WatchOutcome::MatchedMarker { at_seq: bus.last_seq() });
    }

    #[test]
    fn watch_matches_source_activity() {
        let mut bus = EventBus::new();
        bus.publish_coverage("s", "a.rs:1");
        let cursor = bus.subscribe();
        bus.publish_native("s", "menu:open");
        let spec = WatchSpec {
            condition: WatchCondition::SourceActivity(BusSource::Native),
            budget: Duration::from_secs(1),
            poll: Duration::from_millis(10),
        };
        let outcome = spec.run(&bus, cursor);
        assert!(matches!(outcome, WatchOutcome::MatchedSource { .. }));
    }

    #[test]
    fn marker_is_a_distinct_filterable_source() {
        let mut bus = EventBus::new();
        let marker_seq = bus.publish_marker("s", "hello");
        let markers = bus.of_source(0, BusSource::Marker);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].seq, marker_seq);
        if let BusEventKind::Marker(l) = &markers[0].kind {
            assert_eq!(l, "hello");
        } else {
            panic!("expected a marker");
        }
    }
}