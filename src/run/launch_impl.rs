//! Launch specifications: per-session launch specs and derived views.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Record the launch spec of the session this run is attached to
    /// (per-session map, P0 fix 6). The first session recorded becomes the
    /// run's primary (cwd/root resolution).
    pub fn set_launch_spec(&mut self, session_id: &str, spec: crate::session::state::LaunchSpec) {
        if self.primary_session.is_none() {
            self.primary_session = Some(session_id.to_string());
        }
        self.session_specs.insert(session_id.to_string(), spec);
        self.write_manifest().ok();
    }

    /// The launch spec recorded for one session.
    pub fn launch_spec(&self, session_id: &str) -> Option<&crate::session::state::LaunchSpec> {
        self.session_specs.get(session_id)
    }

    /// The primary session's launch spec, if any session was recorded.
    pub fn primary_launch_spec(&self) -> Option<&crate::session::state::LaunchSpec> {
        self.primary_session
            .as_ref()
            .and_then(|id| self.session_specs.get(id))
    }

    /// Replay-history completeness (P0 fix 5): when the in-memory ledger
    /// evicted records before a flush, the durable run is not
    /// replay-complete — the manifest and status report the gap instead of
    /// pretending.
    pub fn history_complete(&self) -> bool {
        self.dropped_records == 0
    }

    pub fn dropped_records(&self) -> u64 {
        self.dropped_records
    }

    pub fn first_available_seq(&self) -> Option<u64> {
        self.first_available_seq
    }

    // ── Contracts (Wave E items 39–49) ──────────────────────────────────

    /// The launch spec cwd of the primary session, if attached.
    pub fn primary_session_cwd(&self) -> Option<&str> {
        self.primary_launch_spec().and_then(|s| s.cwd.as_deref())
    }

    /// Launch specs the run recorded, sorted by session id (replay CLI:
    /// the manifest's per-session launch table).
    pub fn launch_specs(&self) -> Vec<(String, crate::session::state::LaunchSpec)> {
        let mut out: Vec<(String, _)> = self
            .session_specs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Where a `tui://runs/<other-id>` browser read should look for other
    /// persisted runs (Wave G item 75): this run's durable root's parent
    /// (the runs dir) when persisted, then the primary session's cwd. A
    /// durable root whose parent is already the runs dir yields it once.
    pub fn browser_bases(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        if let Some(dir) = &self.run_dir {
            // Durable root is `<runs>/<id>` — the parent is the runs dir.
            if let Some(parent) = dir.parent() {
                out.push(parent.to_path_buf());
            }
        }
        if let Some(cwd) = self.primary_session_cwd() {
            out.push(PathBuf::from(cwd));
        }
        out.dedup();
        out
    }
}
