//! Coverage-ledger methods on [`RunContext`] (impl-family extraction: the
//! struct and its fields stay in `super`; this child module only hosts the
//! method bodies for the native-coverage subsystem).

use super::*;

impl RunContext {
    /// Wave F item 64: fold one native coverage event into the ledger.
    /// Target strings are app-declared (`src/main.rs:42`, `#save.activate`);
    /// the ledger only counts and correlates, never interprets.
    pub fn record_coverage_event(&mut self, session: &str, target: &str) -> anyhow::Result<()> {
        self.ensure_open()?;
        self.coverage.record(session, target, now_ms());
        Ok(())
    }

    /// Wave 5 item 41: fold one coverage event WITH the declaring
    /// component's app-attested source locus. The locus comes from the
    /// same NativeSemanticProtocol channel (the node's `source` field), so
    /// linking widget target → file:line is a join of two app-declared
    /// facts, not an inference — the provenance tier (review §4) stamps
    /// such loci `attested`, the only tier that clears the actionable
    /// fence. Widget-shaped targets accumulate their loci; file targets
    /// keep the locus they already are.
    pub fn record_coverage_event_with_identity(
        &mut self,
        session: &str,
        target: &str,
        mut source_ref: crate::semantic::source_ref::SourceRef,
    ) -> anyhow::Result<()> {
        self.record_coverage_event(session, target)?;
        self.coverage.attach_source_ref(target, source_ref);
        Ok(())
    }

    /// Coverage ingestion from a raw native-event batch. Kept for tests and
    /// callers that hold events directly; the RUNTIME path is
    /// [`Self::ingest_native_coverage_from_events`] — the session absorbs
    /// native events into its queue exactly once (sequence-cursored, ring-
    /// safe), and coverage truth flows from that single stream. The old
    /// whole-channel rescan this replaces double-counted every event on
    /// every call (re-review P0: exactly-once ingestion).
    pub fn ingest_native_coverage_batch(
        &mut self,
        session: &str,
        events: &[crate::semantic::native::EventTuple],
    ) -> usize {
        let mut n = 0;
        for ev in events {
            if ev.event == "coverage" {
                let _ = self.record_coverage_event(session, &ev.target);
                n += 1;
            }
        }
        n
    }

    /// Coverage ingestion from the unified event queue (re-review P1: tool
    /// selection must not affect truth). Fold every absorbed
    /// `NativeEvent { event: "coverage" }` after `cursor` into the ledger,
    /// returning the new cursor. Callers pass a per-run stored cursor so
    /// each event is ingested exactly once, regardless of which observe
    /// modes ran.
    pub fn ingest_native_coverage_from_events(
        &mut self,
        session: &str,
        events: &[crate::events::TerminalEvent],
        mut cursor: usize,
    ) -> usize {
        for ev in events.get(cursor..).unwrap_or(&[]) {
            if let crate::events::TerminalEventKind::NativeEvent { event, target } = &ev.kind {
                if event == "coverage" {
                    let _ = self.record_coverage_event(session, target);
                }
            }
            cursor += 1;
        }
        cursor
    }

    /// A named consumer's event cursor (re-review P1: per-consumer cursors
    /// live in the run so ingestion is exactly-once across tool calls).
    /// Round-2 (G1): stored in the evidence store's event ledger.
    pub fn event_cursor(&self, consumer: &str) -> Option<u64> {
        self.evidence.events.cursor(consumer)
    }

    /// Advance a named consumer's event cursor.
    pub fn set_event_cursor(&mut self, consumer: &str, seq: u64) {
        self.evidence.events.set_cursor(consumer, seq);
    }

    /// The coverage ledger (target → entry), for readers that fold over
    /// all entries. Round-2 (G1): delegates to
    /// [`coverage_state::CoverageState`].
    pub fn coverage_ledger(&self) -> &std::collections::BTreeMap<String, CoverageEntry> {
        self.coverage.entries()
    }

    /// The monotonic coverage sequence (review P0.7).
    pub fn coverage_seq(&self) -> u64 {
        self.coverage.seq()
    }

    /// The caller's last `delta` cursor (review P0.7).
    pub fn coverage_delta_cursor(&self) -> u64 {
        self.coverage.delta_cursor()
    }

    /// Advance the process-wide delta cursor (the historical
    /// single-consumer `delta` contract).
    pub fn set_coverage_delta_cursor(&mut self, v: u64) {
        self.coverage.set_delta_cursor(v);
    }
}
