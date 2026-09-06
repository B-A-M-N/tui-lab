//! Observation state for one session (god-object round 2, G4).
//!
//! The last *settled* observation and the one before it move OUT of the
//! flat `Session` bucket into this holder. The previous→current pairing
//! is observation policy: `observe()` promotes `last` to `previous`
//! exactly once per observation (so `mode=diff` never compares
//! self→self — audit item 12), and a session (re)start clears both.
//!
//! Pure state: no backend, no event queue, no locks. The `Session`
//! facade keeps its `last()` / `previous()` accessors, delegating here.

use crate::screen::ScreenState;

/// The settled-observation window: `(previous, last)`.
#[derive(Default)]
pub(crate) struct ObservationState {
    /// The observation before `last` — set by `observe()` so `mode=diff`
    /// and `diff()` always compare previous→current, never self→self
    /// (audit item 12).
    previous: Option<ScreenState>,
    /// The last *settled* observation (what `observe()` returned).
    last: Option<ScreenState>,
}

impl ObservationState {
    pub(crate) fn new() -> Self {
        ObservationState::default()
    }

    /// Promote the current `last` to `previous` and install the new
    /// observation. Returns the old `last` (the diff base for event
    /// emission), or `None` on the first observation of a generation.
    pub(crate) fn advance(&mut self, current: ScreenState) -> Option<ScreenState> {
        let prev = self.last.take();
        if let Some(p) = &prev {
            self.previous = Some(p.clone());
        }
        self.last = Some(current);
        prev
    }

    /// Reset both windows (session (re)start: a new generation has no
    /// observation history).
    pub(crate) fn clear(&mut self) {
        self.previous = None;
        self.last = None;
    }

    /// The most recent observation.
    pub(crate) fn last(&self) -> Option<&ScreenState> {
        self.last.as_ref()
    }

    /// The observation before `last`, if two observations have been made
    /// since the last (re)start.
    pub(crate) fn previous(&self) -> Option<&ScreenState> {
        self.previous.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(marker: &str) -> ScreenState {
        let mut s = ScreenState::new(10, 2);
        s.viewport_text = vec![marker.to_string(), String::new()];
        s
    }

    #[test]
    fn first_advance_reports_no_diff_base() {
        let mut obs = ObservationState::new();
        assert!(obs.last().is_none());
        assert!(obs.previous().is_none());
        let base = obs.advance(screen("a"));
        assert!(base.is_none(), "first observation has no diff base");
        assert_eq!(obs.last().unwrap().viewport_text[0], "a");
        assert!(obs.previous().is_none());
    }

    #[test]
    fn advance_pairs_previous_and_last() {
        let mut obs = ObservationState::new();
        obs.advance(screen("a"));
        let base = obs.advance(screen("b"));
        assert_eq!(
            base.unwrap().viewport_text[0],
            "a",
            "diff base is the old last"
        );
        assert_eq!(obs.last().unwrap().viewport_text[0], "b");
        assert_eq!(obs.previous().unwrap().viewport_text[0], "a");
        // A third observation shifts the window: a→b→c.
        let base = obs.advance(screen("c"));
        assert_eq!(base.unwrap().viewport_text[0], "b");
        assert_eq!(obs.previous().unwrap().viewport_text[0], "b");
    }

    #[test]
    fn clear_restarts_the_generation() {
        let mut obs = ObservationState::new();
        obs.advance(screen("a"));
        obs.advance(screen("b"));
        obs.clear();
        assert!(obs.last().is_none());
        assert!(obs.previous().is_none());
        let base = obs.advance(screen("c"));
        assert!(base.is_none(), "a new generation starts history over");
    }
}
