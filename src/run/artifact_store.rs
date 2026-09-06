//! Cohesive state for run artifacts and persistence plumbing.
//!
//! Round-2 decomposition (G1): the artifact registry, the ephemeral media
//! queues (held recordings / screen captures awaiting a durable root),
//! the background journal handle, the incremental-ledger watermark, the
//! persistence-health flag, and the restore-damage report move OUT of the
//! flat run bucket into this holder.
//!
//! What stays on the `RunContext` facade: the artifact root path itself
//! (`run_dir` is read by half the run's IO paths and is the one field the
//! promote/restore lifecycle rewrites directly), and all filesystem
//! writes — this store is state, not IO.

use super::{ArtifactRef, JournalHandle, RestoreWarning};

/// Artifact registry + ephemeral media holds + persistence health for one
/// run.
pub(super) struct ArtifactStore {
    /// Artifact registry (Wave B item 15): typed references to every large
    /// artifact the run produced (recordings, event logs). Tools return an
    /// [`ArtifactRef`] instead of inlining multi-kilobyte payloads.
    artifacts: Vec<ArtifactRef>,
    /// Completed PTY recordings retained in memory (run promotion must
    /// carry them into the durable root even when they were stopped while
    /// ephemeral). Keyed by suggested file name.
    held_recordings: Vec<(String, String)>,
    /// Wave F item 57: screen captures held while the run is ephemeral
    /// (name, bytes, format).
    held_captures: Vec<(String, Vec<u8>, String)>,
    /// Background journal writer (audit item: run-journal writer). The
    /// driving path hands pre-serialized ledger lines to a dedicated thread
    /// and never waits on the filesystem; `flush`/`Drop` drain it. `None`
    /// for ephemeral runs (nothing is written) and for restored runs
    /// until their first new record.
    journal: Option<JournalHandle>,
    /// True once the ledger has been appended to transactions.jsonl for a
    /// persistent run (drives incremental appends, Wave B item 14).
    /// SUPERSEDED as the flush authority by `journal` (background writer,
    /// audit item: run-journal writer): the watermark now lives in the
    /// writer and `flush` drains through it. Kept as the restore-time
    /// watermark when a run is re-opened from an existing file.
    ledger_flushed_upto: u64,
    /// True when ledger persistence has failed (writer spawn error or a
    /// terminal write error) — surfaced by `flush` and status instead of
    /// silently degrading to memory-only.
    persistence_unhealthy: bool,
    /// Audit P1-46: artifacts that could NOT be restored, with the reason —
    /// a damaged run comes back usable but DEGRADED, and the agent must
    /// know its evidence is incomplete. Populated only by `restore`;
    /// a live run always starts empty.
    restore_warnings: Vec<RestoreWarning>,
}

impl ArtifactStore {
    /// A fresh, empty store.
    pub(super) fn new() -> Self {
        ArtifactStore {
            artifacts: Vec::new(),
            held_recordings: Vec::new(),
            held_captures: Vec::new(),
            journal: None,
            ledger_flushed_upto: 0,
            persistence_unhealthy: false,
            restore_warnings: Vec::new(),
        }
    }

    /// Append one registered artifact.
    pub(super) fn push_artifact(&mut self, r: ArtifactRef) {
        self.artifacts.push(r);
    }

    /// All registered artifacts.
    pub(super) fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    /// How many artifacts are registered (the next id's base).
    pub(super) fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    /// Retain a completed PTY recording (name + ndjson body) in memory.
    pub(super) fn hold_recording(&mut self, file_name: String, ndjson: String) {
        self.held_recordings.push((file_name, ndjson));
    }

    /// The held recordings.
    pub(super) fn held_recordings(&self) -> &[(String, String)] {
        &self.held_recordings
    }

    /// Take the held recordings (flush/promotion drain).
    pub(super) fn take_held_recordings(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.held_recordings)
    }

    /// Restore the held recordings that could not be written.
    pub(super) fn set_held_recordings(&mut self, held: Vec<(String, String)>) {
        self.held_recordings = held;
    }

    /// Retain a screen capture (SVG/PNG bytes) while ephemeral.
    pub(super) fn hold_capture(&mut self, file_name: String, body: Vec<u8>, format: &str) {
        self.held_captures
            .push((file_name, body, format.to_string()));
    }

    /// Take the held captures (flush/promotion drain).
    pub(super) fn take_held_captures(&mut self) -> Vec<(String, Vec<u8>, String)> {
        std::mem::take(&mut self.held_captures)
    }

    /// Restore the held captures that could not be written.
    pub(super) fn set_held_captures(&mut self, held: Vec<(String, Vec<u8>, String)>) {
        self.held_captures = held;
    }

    /// The background journal writer, if one is running.
    pub(super) fn journal(&self) -> Option<&JournalHandle> {
        self.journal.as_ref()
    }

    /// Install the journal writer (lazy spawn at first record).
    pub(super) fn set_journal(&mut self, j: JournalHandle) {
        self.journal = Some(j);
    }

    /// Whether a journal writer is running.
    pub(super) fn has_journal(&self) -> bool {
        self.journal.is_some()
    }

    /// The incremental-ledger watermark (restore-time / drained value).
    pub(super) fn ledger_flushed_upto(&self) -> u64 {
        self.ledger_flushed_upto
    }

    pub(super) fn set_ledger_flushed_upto(&mut self, v: u64) {
        self.ledger_flushed_upto = v;
    }

    /// Flag ledger persistence as failed (degrade to memory-only, visibly).
    pub(super) fn mark_unhealthy(&mut self) {
        self.persistence_unhealthy = true;
    }

    /// The stored unhealthy flag (the journal's own health is checked
    /// alongside this on the facade's `persistence_unhealthy()`).
    pub(super) fn unhealthy(&self) -> bool {
        self.persistence_unhealthy
    }

    /// Record one restore-time damage report (audit P1-46).
    pub(super) fn note_restore_warning(&mut self, w: RestoreWarning) {
        self.restore_warnings.push(w);
    }

    /// The restore-damage report (empty on live runs).
    pub(super) fn restore_warnings(&self) -> &[RestoreWarning] {
        &self.restore_warnings
    }

    /// Whether anything failed to restore.
    pub(super) fn restore_degraded(&self) -> bool {
        !self.restore_warnings.is_empty()
    }
}
