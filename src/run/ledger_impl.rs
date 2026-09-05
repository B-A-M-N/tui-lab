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
            "primary_session_cwd": self.primary_session_cwd().map(str::to_string),
            "counts": self.counts(),
            // FrameAnalysis storage (re-review item 51): hot ring health —
            // what's resident, what was evicted to the cold log.
            "frames": {
                "hot_resident": self.frame_hot.len(),
                "hot_evicted": self.frame_hot_evicted,
                "next_frame_id": self.next_frame_id,
            },
            "contract": self.contract.as_ref().map(|c| json!({
                "name": c.schema.name,
                "version": c.schema.version,
                "path": self.contract_path,
                "components": c.components.len(),
                "interactions": c.interactions.len(),
                "oracles": c.oracles.len(),
            })),
            "contract_baselines": self.contract_baselines.keys().cloned().collect::<Vec<_>>(),
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
