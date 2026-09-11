//! Transaction ledger, sequence counters, and run status/counts.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Count one observation/wait event.
    pub fn bump_event(&mut self) {
        self.evidence.events.bump();
    }

    /// Count one interaction transaction (act or wait).
    pub fn bump_transaction(&mut self) {
        self.evidence.transactions.bump();
    }

    /// Record one settled interaction transaction into the run's ledger
    /// (re-review Wave-2 item 15). Evidence-level: hashes, settle outcome,
    /// timing, cell delta — not the full frames. The record's `seq` is
    /// assigned here from the run's own counter.
    ///
    /// Bounded (P0 fix 5): when the ledger is full the oldest half is
    /// evicted, and the eviction is *declared* — `dropped_records` grows
    /// and `first_available_seq` moves, so a later flush/promotion can
    /// never present the run as replay-complete.
    // Audit finding 9: the causal event range comes from the TRANSACTION
    // itself (`tx.event_seq_before` / `tx.event_seq_after`, captured at the
    // final dispatch boundary and after the final ingest/native fold in the
    // executor) — callers cannot accidentally supply the backend's
    // `output_seq` counter family anymore. The old separate parameters are
    // gone; the sink copies the transaction's own fields.
    pub fn record_interaction(
        &mut self,
        session: &str,
        generation: u32,
        before_frame_id: Option<u64>,
        after_frame_id: Option<u64>,
        tx: &crate::execution::InteractionTransaction,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        let seq = self.evidence.transactions.bump();
        let record = TransactionRecord::from_interaction(
            seq,
            session,
            generation,
            before_frame_id,
            after_frame_id,
            tx,
        );
        self.push_ledger(record);
        // Focus graph from transactions (audit item): every driving path
        // lands here with before/after frames, so the ID-keyed graph gets
        // its edges from causal evidence — the input that actually moved
        // focus — not from summary-mode polling. The evidence is the
        // transaction's OWN fused analysis (computed at execution time
        // through the session cache + native overlay); re-running bare
        // inference here threw away native facts and could disagree with
        // observe-time focus. `via` is the action's EXACT signature
        // (`tab`, `ctrl+c`, `mouse:left:click@12,3`) — `name()` alone said
        // only "key", which flattened navigation provenance to uselessness
        // (re-review P0).
        let (before_focus, after_focus) = (tx.focus_before.clone(), tx.focus_after.clone());
        if before_focus.as_ref().map(|f| &f.0) != after_focus.as_ref().map(|f| &f.0) {
            if let (Some((Some(f), _)), Some((Some(t), _))) = (before_focus, after_focus) {
                self.graphs
                    .focus_graph
                    .transition(&f, &t, None, &tx.signature());
            }
            // Legacy label ledger for display continuity (label pair form).
            let (b_label, a_label) = (
                tx.focus_before.as_ref().and_then(|f| f.1.clone()),
                tx.focus_after.as_ref().and_then(|f| f.1.clone()),
            );
            if b_label != a_label {
                self.graphs.focus_transitions.push((
                    now_ms(),
                    session.to_string(),
                    b_label,
                    a_label,
                ));
            }
        }
        Ok(())
    }

    /// The sequence assigned to the most recent accepted ledger entry.
    /// Interaction callers should capture this return value (via the
    /// evidence sink) rather than matching rows backward.
    pub fn last_transaction_seq(&self) -> Option<u64> {
        self.evidence.transactions.records().last().map(|r| r.seq)
    }

    /// Record a non-frame interaction (wait/observe) at evidence level.
    ///
    /// Settlement is honestly `skipped`: no settle wait ran for this entry,
    /// and the old hardcoded `settled: true` (with empty hashes) was a lie.
    /// Same bounding + declared eviction as [`Self::record_interaction`]
    /// (P0 fix 5) — one bounded path, not one bounded and one unbounded.
    pub fn record_event(&mut self, session: &str, action: &str) -> anyhow::Result<()> {
        self.ensure_open()?;
        let seq = self.evidence.transactions.bump();
        self.push_ledger(TransactionRecord {
            seq,
            at: now_ms(),
            session: session.to_string(),
            action: action.to_string(),
            settle: "skipped".to_string(),
            settle_reason: Some("non-interaction event".to_string()),
            before_structure: String::new(),
            after_structure: String::new(),
            changed_cells: 0,
            elapsed_ms: 0,
            send_ms: 0,
            settle_ms: 0,
            persisted_action: None,
            render: None,
            origin: None,
            generation: None,
            before_frame_id: None,
            after_frame_id: None,
            event_seq_before: None,
            event_seq_after: None,
            native_revision_before: None,
            before_semantic_identity: None,
            record_kind: Some("event".to_string()),
            dispatch: None,
            dispatch_failure: None,
        });
        Ok(())
    }

    /// The transaction ledger (bounded; see `Self::record_transaction`).
    pub fn transactions(&self) -> &[TransactionRecord] {
        self.evidence.transactions.records()
    }

    /// True transaction count, including ledger entries already evicted.
    pub fn transaction_total(&self) -> u64 {
        self.evidence.transactions.total()
    }

    /// Status snapshot for `tui_run status` (goal spec shape).
    ///
    /// `sessions` comes from the caller: the run records the primary launch
    /// spec; the live session list lives in the SessionManager, which the run
    /// does not own. The MCP layer fills it in.
    pub fn status(&self, sessions: Vec<serde_json::Value>) -> serde_json::Value {
        let journal = self.artifacts_store.journal().map(|j| j.health_snapshot());
        json!({
            "run_id": self.id(),
            "mode": if self.run_dir.is_some() { "persistent" } else { "ephemeral" },
            "persistent": self.run_dir.is_some(),
            "artifact_root": self.run_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
            "sessions": sessions,
            "started_at": self.identity.started_at(),
            "closed": self.identity.closed(),
            // Finding 32: which resume epoch this run is in (0 = the
            // original process's run; >0 = reopened that many times).
            "resume_epoch": self.identity.resume_epoch(),
            "primary_session_cwd": self.primary_session_cwd().map(str::to_string),
            "counts": self.counts(),
            // FrameAnalysis storage (re-review item 51): hot ring health —
            // what's resident, what was evicted to the cold log.
            "frames": {
                "hot_resident": self.evidence.frames.resident(),
                "hot_evicted": self.evidence.frames.evicted(),
                "next_frame_id": self.evidence.frames.next_id(),
            },
            "contract": self.contract.contract().map(|c| json!({
                "name": c.schema.name,
                "version": c.schema.version,
                "path": self.contract.path(),
                "components": c.components.len(),
                "interactions": c.interactions.len(),
                "oracles": c.oracles.len(),
            })),
            "contract_baselines": self.contract.baselines().keys().cloned().collect::<Vec<_>>(),
            "artifacts": self.artifacts_store.artifacts().iter().map(|a| serde_json::json!({
                "id": a.id,
                "kind": a.kind,
                "path": a.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "size": a.size,
                "summary": a.summary,
            })).collect::<Vec<_>>(),
            "journal": journal,
            // Audit P1-46: restored runs report their damage — what the
            // restorer could not bring back. Absent on fresh runs (null).
            // Audit P0 (beta stability, finding 2): the condition was
            // INVERTED — a damaged run reported restore:null (its warnings
            // hidden) and only a healthy run reported
            // {degraded:false,...}. A damaged run must surface its
            // warnings on every status surface.
            "restore": if self.artifacts_store.restore_degraded() {
                self.restore_health()
            } else {
                serde_json::Value::Null
            },
        })
    }

    /// Finding 33: what would be LOST if this run were dropped right
    /// now — the evidence that exists only in memory. For a persistent
    /// run everything durable already sits under the artifact root, so
    /// the loss is the un-flushed held tail (events held for flush,
    /// unsaved recordings, torn checkpoint state); for an ephemeral run
    /// it is the WHOLE in-memory bundle. `tui_run new` refuses over a
    /// non-zero loss unless the caller passes `discard=true`.
    ///
    /// Deliberately conservative: only counts that mean real evidence
    /// (transactions, events, checkpoints, scenarios, findings,
    /// recordings, focus/state-graph edges) — bookkeeping like artifact
    /// refs or contract baselines never blocks a `new`.
    pub fn unsaved_evidence(&self) -> serde_json::Value {
        let incremental_persisted: u64 = self.evidence.events.flushed_total();
        // Events that were counted but never written anywhere yet (an
        // ephemeral run's whole event history, or a persistent run's
        // held tail): counted events minus those already flushed.
        let events_unwritten = self
            .evidence
            .events
            .count()
            .saturating_sub(incremental_persisted);
        let counts = [
            ("transactions", self.evidence.transactions.total()),
            ("events_unwritten", events_unwritten),
            ("checkpoints", self.checkpoints.count() as u64),
            ("scenarios", self.scenarios.len() as u64),
            ("findings", self.findings.len() as u64),
            (
                "held_recordings",
                self.artifacts_store.held_recordings().len() as u64,
            ),
            (
                "focus_transitions",
                self.graphs.focus_transitions.len() as u64,
            ),
            (
                "state_graph_records",
                (self.graphs.state_graph.state_count() + self.graphs.state_graph.transition_count())
                    as u64,
            ),
        ];
        let evidence: serde_json::Value = counts
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect::<serde_json::Value>();
        let lost: u64 = counts.iter().map(|(_, v)| *v).sum();
        json!({
            "lost_if_dropped": lost,
            "persistent": self.run_dir.is_some(),
            // Which buckets actually hold something — a caller refusing
            // (or a caller discarding) sees WHY without diffing counts.
            "evidence": evidence,
        })
    }

    /// Counts for `tui_run status`.
    pub fn counts(&self) -> serde_json::Value {
        json!({
            "transactions": self.transactions().iter().filter(|r| r.record_kind.as_deref() == Some("interaction")).count() as u64,
            "transaction_records": self.evidence.transactions.total(),
            "event_records": self.transactions().iter().filter(|r| r.record_kind.as_deref() == Some("event")).count() as u64,
            "legacy_unclassified_records": self.transactions().iter().filter(|r| r.record_kind.is_none()).count() as u64,
            "transactions_in_ledger": self.transactions().len(),
            "history_complete": self.history_complete(),
            "dropped_records": self.evidence.transactions.dropped_records(),
            "first_available_seq": self.evidence.transactions.first_available_seq(),
            "events": self.evidence.events.count(),
            "events_persisted_incrementally": self.evidence.events.flushed_total(),
            "events_held_for_flush": self.evidence.events.held_count(),
            "event_history_incomplete": self.event_history_incomplete(),
            "event_gaps": self.event_gaps().iter().map(|(consumer, first)| json!({
                "consumer": consumer,
                "first_available_seq": first,
            })).collect::<Vec<_>>(),
            "checkpoints": self.checkpoints.count(),
            "scenarios": self.scenarios.len(),
            "findings": self.findings.len(),
            "held_recordings": self.artifacts_store.held_recordings().len(),
            "focus_transitions": self.graphs.focus_transitions.len(),
            "focus_graph_edges": self.graphs.focus_graph.edges.len(),
            "state_graph_states": self.graphs.state_graph.state_count(),
            "state_graph_transitions": self.graphs.state_graph.transition_count(),
            "frames": self.evidence.frames.next_id(),
            "artifacts": self.artifacts_store.artifacts().len(),
        })
    }
}

#[cfg(test)]
mod event_gap_tests {
    use super::*;

    #[test]
    fn event_ring_gap_is_declared_and_status_becomes_incomplete() {
        let mut run = RunContext::ephemeral();
        assert!(!run.event_history_incomplete());
        assert!(run.event_gaps().is_empty());

        // A fold consumer that crossed an evicted prefix records the
        // available tail but cannot claim exhaustive evidence.
        run.note_event_gap("persistence:sess-gap", Some(17));
        assert!(run.event_history_incomplete());
        assert_eq!(run.event_gaps().len(), 1);
        assert_eq!(run.event_gaps()[0].0, "persistence:sess-gap");
        assert_eq!(run.event_gaps()[0].1, Some(17));

        let status = run.counts();
        assert_eq!(status["event_history_incomplete"], true);
        assert_eq!(status["event_gaps"][0]["consumer"], "persistence:sess-gap");
        assert_eq!(status["event_gaps"][0]["first_available_seq"], 17);
    }
}

#[cfg(test)]
mod record_kind_tests {
    use super::*;

    #[test]
    fn ledger_records_are_explicitly_typed_and_counted() {
        let mut run = RunContext::ephemeral();
        let tx = crate::execution::InteractionTransaction {
            action: crate::execution::ActionEnvelope::new(
                crate::execution::CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
                },
                crate::execution::InputVisibility::Normal,
            ),
            anchor: Default::default(),
            before_frame: crate::backend::CanonicalFrame::new(
                crate::screen::ScreenState::new(2, 1),
                1,
                1,
            ),
            after_frame: crate::backend::CanonicalFrame::new(
                crate::screen::ScreenState::new(2, 1),
                1,
                2,
            ),
            settle: crate::execution::SettleStatus::Met,
            transition: crate::screen::diff::diff(
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(2, 1), 1, 1)
                    .state,
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(2, 1), 1, 2)
                    .state,
            ),
            capture: None,
            focus_before: None,
            focus_after: None,
            elapsed_ms: 1,
            send_ms: 1,
            settle_ms: 0,
            render: None,
            transition_capture: None,
            origin: Some(crate::execution::DriveOrigin::Act),
            dispatch: crate::execution::DispatchStatus::Sent,
            dispatch_failure: None,
            event_seq_before: 0,
            event_seq_after: Some(1),
            native_revision_before: None,
            dispatch_reason: None,
        };
        run.record_interaction("s", 1, None, None, &tx).unwrap();
        run.record_event("s", "wait").unwrap();
        run.record_event("s", "observe").unwrap();
        let rows = run.transactions();
        assert_eq!(rows[0].record_kind.as_deref(), Some("interaction"));
        assert_eq!(rows[1].record_kind.as_deref(), Some("event"));
        assert_eq!(rows[2].record_kind.as_deref(), Some("event"));
        let counts = run.counts();
        assert_eq!(counts["transactions"], 1);
        assert_eq!(counts["transaction_records"], 3);
        assert_eq!(counts["event_records"], 2);
        assert_eq!(counts["legacy_unclassified_records"], 0);
    }
}
