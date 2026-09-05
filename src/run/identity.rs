//! Cohesive state for run identity and session launch provenance.
//!
//! Round-2 decomposition (G1): the four identity fields and the two
//! session-provenance fields move OUT of the flat run bucket. Identity is
//! who this run is; the session registry is which sessions it has seen
//! and how they were launched — provenance records, not live process
//! ownership (live sessions belong to the SessionManager, which the run
//! does not own). Filesystem persistence stays on the `RunContext`
//! facade: these holders are state, not IO.
//!
//! `RunContext` keeps the public methods (and the `id`/`started_at`
//! accessors external callers use) as thin delegations.

use std::collections::HashMap;

/// Run identity: who this run is and where it sits in its lifecycle.
pub(super) struct RunIdentity {
    /// Opaque run id (`run-<uuid simple>`).
    id: String,
    /// Wall-clock start (unix millis).
    started_at: u64,
    /// Set by `tui_run close`. Sessions are NOT touched by closing.
    closed: bool,
    /// Resume epoch (finding 32): 0 for a run in its original process,
    /// incremented each time `reopen()` brings a closed persisted run
    /// back to life. A reopened run is NOT the original process's run —
    /// its later records live after a process boundary it did not choose.
    /// Evidence recorded under epoch > 0 can be distinguished from the
    /// original history (manifest + status carry the epoch; `reopen`
    /// writes it down before anything else runs).
    resume_epoch: u64,
}

impl RunIdentity {
    /// A fresh identity for a new run.
    pub(super) fn fresh(id: String, started_at: u64) -> Self {
        RunIdentity {
            id,
            started_at,
            closed: false,
            resume_epoch: 0,
        }
    }

    /// Adopt a restored run's identity (the manifest is authoritative).
    pub(super) fn adopt(&mut self, id: String, started_at: u64, closed: bool, resume_epoch: u64) {
        self.id = id;
        self.started_at = started_at;
        self.closed = closed;
        self.resume_epoch = resume_epoch;
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn started_at(&self) -> u64 {
        self.started_at
    }

    pub(super) fn closed(&self) -> bool {
        self.closed
    }

    pub(super) fn set_closed(&mut self, v: bool) {
        self.closed = v;
    }

    pub(super) fn resume_epoch(&self) -> u64 {
        self.resume_epoch
    }

    pub(super) fn set_resume_epoch(&mut self, v: u64) {
        self.resume_epoch = v;
    }
}

/// Session launch provenance: one launch spec per session id the run has
/// seen (re-review P0 fix 6 — each restart creates a new generation), the
/// first recorded session being the run's primary for cwd/root
/// resolution.
#[derive(Default)]
pub(super) struct RunSessionRegistry {
    /// Launch spec per session.
    specs: HashMap<String, crate::session::state::LaunchSpec>,
    /// First session recorded — the run's primary.
    primary: Option<String>,
}

impl RunSessionRegistry {
    /// Record the launch spec of one session; the first recorded session
    /// becomes the run's primary.
    pub(super) fn record_launch(
        &mut self,
        session_id: &str,
        spec: crate::session::state::LaunchSpec,
    ) {
        if self.primary.is_none() {
            self.primary = Some(session_id.to_string());
        }
        self.specs.insert(session_id.to_string(), spec);
    }

    /// Adopt a manifest's session table (restore path; the manifest is
    /// authoritative).
    pub(super) fn adopt(
        &mut self,
        specs: HashMap<String, crate::session::state::LaunchSpec>,
        primary: Option<String>,
    ) {
        self.specs = specs;
        self.primary = primary;
    }

    pub(super) fn spec(&self, session_id: &str) -> Option<&crate::session::state::LaunchSpec> {
        self.specs.get(session_id)
    }

    pub(super) fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    /// The primary session's launch spec, if any session was recorded.
    pub(super) fn primary_spec(&self) -> Option<&crate::session::state::LaunchSpec> {
        self.primary.as_ref().and_then(|id| self.specs.get(id))
    }

    /// Session ids, sorted (stable display / replay tables).
    pub(super) fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.specs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// (session id, spec) pairs, sorted by session id.
    pub(super) fn pairs(&self) -> Vec<(String, crate::session::state::LaunchSpec)> {
        self.specs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The full spec table (manifest serialization).
    pub(super) fn all_specs(&self) -> &HashMap<String, crate::session::state::LaunchSpec> {
        &self.specs
    }
}
