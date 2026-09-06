//! Cohesive state for the native-coverage domain.
//!
//! Round-2 decomposition (G1): the three `RunContext` fields that make up
//! interaction-correlated coverage — the target ledger, the monotonic
//! sequence, and the process-wide delta cursor — move OUT of the flat run
//! bucket into this holder. The delta cursor is a semantic policy choice
//! (review P0.7), not generic run bookkeeping, so it lives next to the
//! ledger it cursors. Generic per-consumer event cursors do NOT live here;
//! they moved to the evidence store's event ledger.
//!
//! `RunContext` keeps the public methods as thin delegations; the
//! closed-run guard (`ensure_open`) stays on the facade. On-disk format
//! unchanged: `coverage.json` is still a plain `BTreeMap<String,
//! CoverageEntry>` and `coverage_seq` is still not persisted — it is
//! RECONSTRUCTED on restore as the entries' max `last_seq` (audit P1,
//! coverage sequence identity), so post-resume events continue strictly
//! above every pre-close cursor instead of restarting at 0. Pre-cursor-era
//! files (all seqs 0) restore to 0, which the summary note reports.

use super::CoverageEntry;
use std::collections::BTreeMap;

/// Native coverage ledger + sequence + delta cursor for one run.
#[derive(Default)]
pub(super) struct CoverageState {
    /// Wave F item 64: target → hit statistics. Entries accumulate from
    /// NativeSemanticProtocol `coverage` events: target → (hits, sessions).
    /// This is interaction-correlated coverage: "did my last act exercise
    /// new app code" is answerable by diffing before/after a step.
    ledger: BTreeMap<String, CoverageEntry>,
    /// Monotonic coverage sequence (review P0.7). Incremented on every
    /// ledger insert/update so a real `delta` cursor exists. Defaults to 0
    /// (pre-P0.7 persisted runs) — every target then reads as "new" on the
    /// first delta after upgrade, which is honest.
    seq: u64,
    /// The caller's last `delta` cursor (review P0.7): the `seq` value from
    /// the previous delta. Targets with `first_seq >` this are
    /// newly-reported since the previous delta. Without more state we
    /// cannot represent per-caller cursors; this is the single
    /// process-wide cursor the MCP tool advances on each delta call and
    /// reports in the response.
    delta_cursor: u64,
}

impl CoverageState {
    /// A fresh, empty state.
    pub(super) fn new() -> Self {
        CoverageState::default()
    }

    /// Fold one coverage hit for `target`. Target strings are app-declared
    /// (`src/main.rs:42`, `#save.activate`); the ledger only counts and
    /// correlates, never interprets.
    pub(super) fn record(&mut self, session: &str, target: &str, now: u64) {
        if target.is_empty() {
            return;
        }
        // Monotonic coverage sequence (review P0.7): each hit advances it so
        // delta can tell "new since my last delta" from "hit again".
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        let entry = self
            .ledger
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
    }

    /// Attach an app-attested source locus to one target's entry. The
    /// locus comes from the same NativeSemanticProtocol channel (the
    /// node's `source` field), so the join is exact, not inferred; the
    /// provenance tier stamps it `attested` — the only tier that clears
    /// the actionable fence (Wave 5 item 41).
    pub(super) fn attach_source_ref(
        &mut self,
        target: &str,
        source_ref: crate::semantic::source_ref::SourceRef,
    ) {
        let Some(entry) = self.ledger.get_mut(target) else {
            return;
        };
        let mut r = source_ref;
        r.provenance = crate::semantic::source_ref::Provenance::Attested;
        if !entry
            .source_refs
            .iter()
            .any(|e| e.location() == r.location())
        {
            entry.source_refs.push(r);
        }
    }

    /// The ledger, target-ordered.
    pub(super) fn entries(&self) -> &BTreeMap<String, CoverageEntry> {
        &self.ledger
    }

    /// Adopt a loaded ledger (restore path; the file is authoritative).
    /// The sequence high-water mark is RECONSTRUCTED from the entries'
    /// `last_seq` — a fresh `seq = 0` after restore would hand new
    /// post-resume events sequence values BELOW the previous epoch's
    /// cursor, so a client holding a pre-close coverage cursor would
    /// never see them again (audit P1, coverage sequence identity).
    /// With the high-water restored, post-resume `record()`s continue
    /// strictly above every persisted sequence, and the run's delta
    /// cursor starts at the high-water too: a post-resume delta reports
    /// only genuinely new targets, not the whole restored ledger as
    /// "new". (Pre-cursor-era files — every seq 0 — stay at 0, which the
    /// summary's honest note still reports.)
    pub(super) fn set_entries(&mut self, ledger: BTreeMap<String, CoverageEntry>) {
        let high_water = ledger.values().map(|e| e.last_seq).max().unwrap_or(0);
        self.seq = high_water;
        self.delta_cursor = high_water;
        self.ledger = ledger;
    }

    pub(super) fn seq(&self) -> u64 {
        self.seq
    }

    pub(super) fn delta_cursor(&self) -> u64 {
        self.delta_cursor
    }

    pub(super) fn set_delta_cursor(&mut self, v: u64) {
        self.delta_cursor = v;
    }
}
