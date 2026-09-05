//! Cohesive state for citable execution evidence.
//!
//! Round-2 decomposition (G1): the nine `RunContext` fields that make up
//! the evidence ledgers — interaction transactions, committed frames, and
//! observed terminal events — move OUT of the flat run bucket into this
//! holder, internally split along the plan's three-ledger shape:
//!
//! ```text
//! EvidenceStore
//! ├── TransactionLedger  records, lifetime_count, dropped, first seq
//! ├── FrameLedger        next id, hot ring, evicted count
//! └── EventLedger        count, held backlog, per-session flush counts
//! ```
//!
//! `RunContext` keeps the public methods as thin delegations so every
//! caller compiles unchanged. Pure ledger policy (bounds, eviction,
//! counters) lives here; filesystem writes and journal/thread plumbing
//! stay on the `RunContext` facade — this store is state, not IO.

use super::{
    TransactionRecord, FRAME_HOT_RING, TRANSACTION_RING_HIGH_WATER, TRANSACTION_RING_LOW_WATER,
};
use std::collections::{HashMap, VecDeque};

/// Transactions + observed-event + frame ledgers for one run.
pub(super) struct EvidenceStore {
    pub(super) transactions: TransactionLedger,
    pub(super) frames: FrameLedger,
    pub(super) events: EventLedger,
}

impl EvidenceStore {
    /// A fresh, empty store.
    pub(super) fn new() -> Self {
        EvidenceStore {
            transactions: TransactionLedger::new(),
            frames: FrameLedger::new(),
            events: EventLedger::new(),
        }
    }
}

/// The interaction-transaction ledger: a serializable record of every
/// interaction transaction, so the run can *reconstruct* what happened
/// (`transactions: 47` without 47 reconstructable transactions was the old
/// shape). Bounded; the full frames stay in the callers' hands — this is
/// the evidence-level record.
pub(super) struct TransactionLedger {
    /// Bounded in-memory window of settled transactions.
    records: Vec<TransactionRecord>,
    /// Interaction transactions executed in this run (act/wait counters) —
    /// the TRUE count, including entries already evicted from the window.
    lifetime_count: u64,
    /// Declared eviction (P0 fix 5): records evicted from the window (or
    /// torn ledger lines at restore). The run's manifest and status carry
    /// this, so a promoted run never pretends to be replay-complete.
    dropped_records: u64,
    /// Seq of the oldest record still resident (`None` = full history).
    first_available_seq: Option<u64>,
}

impl TransactionLedger {
    fn new() -> Self {
        TransactionLedger {
            records: Vec::new(),
            lifetime_count: 0,
            dropped_records: 0,
            first_available_seq: None,
        }
    }

    /// Consume the next sequence number (act/wait counter).
    pub(super) fn bump(&mut self) -> u64 {
        let seq = self.lifetime_count;
        self.lifetime_count += 1;
        seq
    }

    /// The bounded in-memory window (oldest first).
    pub(super) fn records(&self) -> &[TransactionRecord] {
        &self.records
    }

    /// True transaction count, including evicted entries.
    pub(super) fn total(&self) -> u64 {
        self.lifetime_count
    }

    /// Declared eviction accounting for records that never entered (or
    /// fell out of) the in-memory window — torn restore lines, for
    /// example. `first_available_seq` is untouched unless the window is
    /// empty (nothing older remains resident).
    pub(super) fn note_dropped(&mut self, n: u64) {
        self.dropped_records += n;
        self.first_available_seq = self.records.first().map(|r| r.seq);
    }

    /// Restore-path adoption: adopt one record read back from disk,
    /// keeping the lifetime count consistent with the highest seq seen.
    pub(super) fn adopt_restored(&mut self, rec: TransactionRecord) {
        self.lifetime_count = self.lifetime_count.max(rec.seq + 1);
        self.records.push(rec);
    }

    /// Shared bounded push with declared eviction (P0 fix 5). The window
    /// is a high-water/low-water pair — trim fires past HIGH and drains
    /// back to LOW, so the resident bound is exactly HIGH (never
    /// HIGH + LOW as the old `MAX + 512` trigger allowed).
    pub(super) fn push(&mut self, record: TransactionRecord) {
        self.records.push(record);
        if self.records.len() >= TRANSACTION_RING_HIGH_WATER {
            let excess = self.records.len() - TRANSACTION_RING_LOW_WATER;
            let new_first = self.records[excess].seq;
            self.records.drain(..excess);
            self.dropped_records += excess as u64;
            self.first_available_seq = Some(new_first);
        }
    }

    pub(super) fn dropped_records(&self) -> u64 {
        self.dropped_records
    }

    pub(super) fn set_dropped_records(&mut self, v: u64) {
        self.dropped_records = v;
    }

    pub(super) fn first_available_seq(&self) -> Option<u64> {
        self.first_available_seq
    }

    pub(super) fn set_first_available_seq(&mut self, v: Option<u64>) {
        self.first_available_seq = v;
    }
}

/// The frame ledger: the per-run `frame:N` id allocator plus the bounded
/// HOT ring of per-frame records — ids, sequences, hashes, capture time,
/// commit timing — queryable by citable id without touching the
/// filesystem. The COLD half (the full `ScreenState` grid) never lives
/// here: it stays with the frame's owner (session/transaction) and, for
/// persistent runs, in `frames.jsonl`. Ring eviction is FIFO past
/// [`FRAME_HOT_RING`] entries; evicted ids remain resolvable through the
/// cold log, and the ring reports its own eviction count so a miss is
/// diagnosable, not silent.
pub(super) struct FrameLedger {
    /// Per-run frame id allocator (Wave B item 11).
    next_id: u64,
    /// The HOT ring, oldest first.
    hot: VecDeque<super::FrameRecord>,
    /// How many hot records have been evicted past the ring bound.
    evicted: u64,
}

impl FrameLedger {
    fn new() -> Self {
        FrameLedger {
            next_id: 0,
            hot: VecDeque::new(),
            evicted: 0,
        }
    }

    /// Allocate the next citable frame id (`frame:N`).
    pub(super) fn allocate_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// File a committed frame's HOT record, evicting FIFO past the ring
    /// bound (declared — see [`Self::evicted`]).
    pub(super) fn push_hot(&mut self, record: super::FrameRecord) {
        self.hot.push_back(record);
        while self.hot.len() > FRAME_HOT_RING {
            self.hot.pop_front();
            self.evicted += 1;
        }
    }

    /// Recall one committed frame's HOT record by citable id. `None` means
    /// not in the hot ring — either never committed to this run, or
    /// evicted; the cold log (`frames.jsonl`, persistent runs) still
    /// resolves it.
    pub(super) fn record(&self, frame_id: u64) -> Option<&super::FrameRecord> {
        self.hot.iter().find(|r| r.frame_id == frame_id)
    }

    /// The hot ring, oldest first.
    pub(super) fn hot(&self) -> impl Iterator<Item = &super::FrameRecord> {
        self.hot.iter()
    }

    /// Hot-ring health for status: resident / evicted / allocated.
    pub(super) fn resident(&self) -> usize {
        self.hot.len()
    }

    pub(super) fn evicted(&self) -> u64 {
        self.evicted
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next_id
    }
}

/// The terminal-event ledger: observed-event counts, the ephemeral-run
/// hold backlog, and per-session incremental flush counts (item 38).
pub(super) struct EventLedger {
    /// Events observed in this run (observe/wait calls).
    count: u64,
    /// Terminal-event batches drained from sessions (Wave B item 12/14):
    /// (session id, events). Filled by the MCP layer before flush; written
    /// to `events/<session>.jsonl` when the run persists. Persistent runs
    /// bypass this backlog (item 38: incremental appends at hold time).
    held: Vec<(String, Vec<crate::events::TerminalEvent>)>,
    /// Events already written durably per session (item 38) — evidence for
    /// status: "events persisted incrementally" vs "held in memory".
    flushed_counts: HashMap<String, u64>,
}

impl EventLedger {
    fn new() -> Self {
        EventLedger {
            count: 0,
            held: Vec::new(),
            flushed_counts: HashMap::new(),
        }
    }

    /// Count one observation/wait event.
    pub(super) fn bump(&mut self) {
        self.count += 1;
    }

    /// Count a drained batch (incrementally persisted).
    pub(super) fn add_batch(&mut self, n: usize) {
        self.count += n as u64;
    }

    pub(super) fn count(&self) -> u64 {
        self.count
    }

    /// Record one batch as durably written for a session (item 38).
    pub(super) fn note_flushed(&mut self, session: &str, n: usize) {
        *self.flushed_counts.entry(session.to_string()).or_insert(0) += n as u64;
    }

    /// Sum of all per-session flush counts.
    pub(super) fn flushed_total(&self) -> u64 {
        self.flushed_counts.values().sum()
    }

    /// Hold one session's batch for the flush-time whole-file write (the
    /// ephemeral-run contract; persistent runs bypass this at hold time).
    pub(super) fn hold(&mut self, session: &str, events: Vec<crate::events::TerminalEvent>) {
        self.held.push((session.to_string(), events));
    }

    /// The held backlog, drained (empty after).
    pub(super) fn take_held(&mut self) -> Vec<(String, Vec<crate::events::TerminalEvent>)> {
        std::mem::take(&mut self.held)
    }

    /// Total events currently held for flush.
    pub(super) fn held_count(&self) -> u64 {
        self.held.iter().map(|(_, e)| e.len() as u64).sum()
    }
}
