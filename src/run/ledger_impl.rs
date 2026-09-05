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
        self.event_count += 1;
    }

    /// Count one interaction transaction (act or wait).
    pub fn bump_transaction(&mut self) {
        self.transaction_count += 1;
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
    pub fn record_interaction(
        &mut self,
        session: &str,
        tx: &crate::execution::InteractionTransaction,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        let seq = self.transaction_count;
        self.transaction_count += 1;
        let mut record = TransactionRecord::from_interaction(seq, session, tx);
        record.seq = seq;
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
                self.focus_graph.transition(&f, &t, None, &tx.signature());
            }
            // Legacy label ledger for display continuity (label pair form).
            let (b_label, a_label) = (
                tx.focus_before.as_ref().and_then(|f| f.1.clone()),
                tx.focus_after.as_ref().and_then(|f| f.1.clone()),
            );
            if b_label != a_label {
                self.focus_transitions
                    .push((now_ms(), session.to_string(), b_label, a_label));
            }
        }
        Ok(())
    }

    /// Record a non-frame interaction (wait/observe) at evidence level.
    ///
    /// Settlement is honestly `skipped`: no settle wait ran for this entry,
    /// and the old hardcoded `settled: true` (with empty hashes) was a lie.
    /// Same bounding + declared eviction as [`Self::record_interaction`]
    /// (P0 fix 5) — one bounded path, not one bounded and one unbounded.
    pub fn record_event(&mut self, session: &str, action: &str) -> anyhow::Result<()> {
        self.ensure_open()?;
        let seq = self.transaction_count;
        self.transaction_count += 1;
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
        });
        Ok(())
    }

    /// The transaction ledger (bounded; see [`Self::record_transaction`]).
    pub fn transactions(&self) -> &[TransactionRecord] {
        &self.transactions
    }

    /// True transaction count, including ledger entries already evicted.
    pub fn transaction_total(&self) -> u64 {
        self.transaction_count
    }

    /// Status snapshot for `tui_run status` (goal spec shape).
    ///
    /// `sessions` comes from the caller: the run records the primary launch
    /// spec; the live session list lives in the SessionManager, which the run
    /// does not own. The MCP layer fills it in.
    pub fn status(&self, sessions: Vec<serde_json::Value>) -> serde_json::Value {
        let journal = self.journal.as_ref().map(|j| j.health_snapshot());
        json!({
            "run_id": self.id,
            "mode": if self.run_dir.is_some() { "persistent" } else { "ephemeral" },
            "persistent": self.run_dir.is_some(),
            "artifact_root": self.run_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
            "sessions": sessions,
            "started_at": self.started_at,
            "closed": self.closed,
            // Finding 32: which resume epoch this run is in (0 = the
            // original process's run; >0 = reopened that many times).
            "resume_epoch": self.resume_epoch,
            "primary_session_cwd": self.primary_session_cwd().map(str::to_string),
            "counts": self.counts(),
            // FrameAnalysis storage (re-review item 51): hot ring health —
            // what's resident, what was evicted to the cold log.
            "frames": {
                "hot_resident": self.frame_hot.len(),
                "hot_evicted": self.frame_hot_evicted,
                "next_frame_id": self.next_frame_id,
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
            "artifacts": self.artifacts.iter().map(|a| serde_json::json!({
                "id": a.id,
                "kind": a.kind,
                "path": a.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "size": a.size,
                "summary": a.summary,
            })).collect::<Vec<_>>(),
            "journal": journal,
            // Audit P1-46: restored runs report their damage — what the
            // restorer could not bring back. Absent on fresh runs (null).
            "restore": if self.restore_warnings.is_empty() {
                serde_json::Value::Null
            } else {
                self.restore_health()
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
        let incremental_persisted: u64 = self.event_flushed_counts.values().sum();
        // Events that were counted but never written anywhere yet (an
        // ephemeral run's whole event history, or a persistent run's
        // held tail): counted events minus those already flushed.
        let events_unwritten = self.event_count.saturating_sub(incremental_persisted);
        let counts = [
            ("transactions", self.transaction_count),
            ("events_unwritten", events_unwritten),
            ("checkpoints", self.checkpoints.count() as u64),
            ("scenarios", self.saved_scenarios.len() as u64),
            ("findings", self.findings.len() as u64),
            ("held_recordings", self.held_recordings.len() as u64),
            ("focus_transitions", self.focus_transitions.len() as u64),
            (
                "state_graph_records",
                (self.state_graph.state_count() + self.state_graph.transition_count()) as u64,
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
        let held: u64 = self.held_events.iter().map(|(_, e)| e.len() as u64).sum();
        json!({
            "transactions": self.transaction_count,
            "transactions_in_ledger": self.transactions.len(),
            "history_complete": self.history_complete(),
            "dropped_records": self.dropped_records,
            "first_available_seq": self.first_available_seq,
            "events": self.event_count,
            "events_persisted_incrementally": self.event_flushed_counts.values().sum::<u64>(),
            "events_held_for_flush": held,
            "checkpoints": self.checkpoints.count(),
            "scenarios": self.saved_scenarios.len(),
            "findings": self.findings.len(),
            "held_recordings": self.held_recordings.len(),
            "focus_transitions": self.focus_transitions.len(),
            "focus_graph_edges": self.focus_graph.edges.len(),
            "state_graph_states": self.state_graph.state_count(),
            "state_graph_transitions": self.state_graph.transition_count(),
            "frames": self.next_frame_id,
            "artifacts": self.artifacts.len(),
        })
    }
}
