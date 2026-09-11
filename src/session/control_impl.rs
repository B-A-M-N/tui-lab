//! Control lease, isolation evidence, anchor sequencing, unrecorded input, capability/terminal-profile reporting.
//!
//! Impl-family extraction (Phase 2): the `Session` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. `Session` remains the single
//! per-session concurrency authority — no independent locking is
//! introduced. Signatures, visibility, and callers are unchanged.

use super::*;

impl Session {
    /// Wave G item 76: take the human control lease (holder + TTL). A live
    /// lease refuses with the current holder's grant.
    pub fn acquire_lease(
        &mut self,
        holder: &str,
        ttl_ms: u64,
    ) -> Result<crate::session::lease::ControlLease, crate::session::lease::ControlLease> {
        self.lease.acquire(holder, ttl_ms)
    }

    /// Release the control lease with its token (audit finding 4).
    /// `Ok(released)` — token matched (or nothing was live);
    /// `Err(LeaseTokenMismatch)` — a LIVE lease is held by someone else
    /// (the lease stands; the error names the holder).
    pub fn release_lease_with_token(
        &mut self,
        token: &str,
    ) -> Result<bool, crate::session::lease::LeaseTokenMismatch> {
        self.lease.release_token(token)
    }

    /// Internal, unconditional release for lifecycle teardown paths
    /// (stop/restart/close) that act with operator authority — NOT the
    /// machine-facing release, which must present the token.
    pub fn release_lease(&mut self) -> bool {
        self.lease.release()
    }

    /// The active lease, if any.
    pub fn active_lease(&mut self) -> Option<crate::session::lease::ControlLease> {
        self.lease.active()
    }

    /// Wave G item 76: does the lease currently block machine driving?
    /// Every driving path checks this before sending input.
    pub fn driving_blocked(&mut self) -> Option<crate::session::lease::ControlLease> {
        self.lease.active()
    }

    /// Beta-audit P0-7: install the run evidence sink for the duration of
    /// one authorized job. While installed, every transaction the
    /// canonical executor produces commits through the ONE pipeline
    /// (ticket-verified frames + run ledger + event/coverage fold) — the
    /// invariants `drive_pipeline` enforces for `tui_act` become
    /// properties of the executor for every driving path.
    pub fn install_evidence_sink(&mut self, sink: crate::execution::RunEvidenceSink) {
        self.evidence_sink = Some(std::sync::Arc::new(sink));
    }

    /// Install an already-shared sink. Authorized dispatch uses this so
    /// the exact captured ticket/run identity is reused everywhere (and a
    /// second commit of an already-committed transaction can detect the
    /// same sink).
    pub fn install_evidence_sink_arc(
        &mut self,
        sink: std::sync::Arc<crate::execution::RunEvidenceSink>,
    ) {
        self.evidence_sink = Some(sink);
    }

    /// Clear the sink (end of the authorized job). Returns what was
    /// installed, for the dispatcher's health reporting.
    pub fn take_evidence_sink(
        &mut self,
    ) -> Option<std::sync::Arc<crate::execution::RunEvidenceSink>> {
        self.evidence_sink.take()
    }

    /// The installed sink, if any (executor read path).
    pub fn evidence_sink(&self) -> Option<std::sync::Arc<crate::execution::RunEvidenceSink>> {
        self.evidence_sink.clone()
    }

    /// Wave G item 77: what the current generation actually launched with.
    pub fn isolation_evidence(&self) -> Option<&crate::session::isolation::IsolationEvidence> {
        self.isolation_evidence.as_ref()
    }

    /// Allocate the next monotonic anchor index for this session
    /// (re-review P1 fix 9). Every [`crate::execution::ObservationAnchor`]
    /// created against this session gets a distinct, increasing index.
    pub fn next_anchor(&mut self) -> u64 {
        self.event_state.next_anchor()
    }

    /// Suppress input recording for the duration of `f` (re-review P0 leak
    /// fix, recording half). Sensitive payloads must not reach the cast file
    /// through [`RecordingHook::on_input`]; the hook forwards bytes
    /// synchronously on the calling thread inside `send_input`, so a guard
    /// flag set/cleared around the send is race-free.
    pub fn send_unrecorded(&mut self, input: crate::backend::Input) -> anyhow::Result<()> {
        self.recording.suppress_input_only();
        let result = self.backend.send_input(input);
        self.recording.resume_input_only();
        result.map_err(anyhow::Error::from)
    }

    /// Live capability query: re-asks the backend, then overlays session-
    /// provisioned capabilities. `native_semantic` is not a transport fact:
    /// it becomes supported only when the session actually created a
    /// channel path and injected its environment pair.
    pub fn capabilities(&mut self) -> Capabilities {
        let mut caps = self.backend.capabilities();
        let native_available = self.native.env_pair().is_some();
        caps.native_semantic = native_available;
        caps
    }

    /// Evidence-backed terminal profile for this session (Wave G review P1/P2
    /// 16): lifts the live [`Capabilities`] into per-feature rows that each
    /// carry the observation that proved (or failed to prove) the capability,
    /// so an agent never assumes mouse/kitty/title that was never negotiated.
    pub fn terminal_profile(&mut self) -> crate::terminal::TerminalProfile {
        let caps = self.capabilities();
        // The backend enumerates observation from the live backend, so no
        // foreign evidence map is needed here.
        crate::terminal::TerminalProfile::build(caps, &std::collections::HashMap::new())
    }

    /// Item 48: apply a project contract's normalization policy to this
    /// session's backend. Every subsequent structure hash normalizes the
    /// contract's `volatile_patterns` on top of the built-in classes, so a
    /// clock the contract declares volatile stops fragmenting the state
    /// graph. Invalid patterns were rejected at contract load; this only
    /// receives compiled policies.
    pub fn set_normalization_policy(
        &mut self,
        policy: std::sync::Arc<crate::screen::NormalizationPolicy>,
    ) {
        self.backend.set_normalization_policy(policy);
    }

    /// Capabilities as they were when the session last (re)started.
    pub fn capabilities_at_start(&self) -> Capabilities {
        self.caps_at_start.clone()
    }
}
