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
        if target.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        // Monotonic coverage sequence (review P0.7): each hit advances it so
        // delta can tell "new since my last delta" from "hit again".
        self.coverage_seq = self.coverage_seq.saturating_add(1);
        let seq = self.coverage_seq;
        let entry = self
            .coverage_ledger
            .entry(target.to_string())
            .or_insert_with(|| CoverageEntry {
                hits: 0,
                sessions: Vec::new(),
                first_seen: now,
                last_seen: now,
                first_seq: seq,
                last_seq: seq,
                source_refs: Vec::new(),
            });
        entry.hits += 1;
        entry.last_seen = now;
        entry.last_seq = seq;
        if !entry.sessions.iter().any(|s| s == session) {
            entry.sessions.push(session.to_string());
        }
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
        let Some(entry) = self.coverage_ledger.get_mut(target) else {
            return Ok(());
        };
        // Native id → coverage event → locus: the app drew every edge.
        source_ref.provenance = crate::semantic::source_ref::Provenance::Attested;
        if !entry
            .source_refs
            .iter()
            .any(|r| r.location() == source_ref.location())
        {
            entry.source_refs.push(source_ref);
        }
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
    pub fn event_cursor(&self, consumer: &str) -> Option<u64> {
        self.event_cursors.get(consumer).copied()
    }

    /// Advance a named consumer's event cursor.
    pub fn set_event_cursor(&mut self, consumer: &str, seq: u64) {
        self.event_cursors.insert(consumer.to_string(), seq);
    }
}
