//! Frame evidence: frame records, commits, held events, focus history.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Record one focus transition observed during any session's traffic
    /// (the run's focus graph, one entry per semantic focus change).
    pub fn record_focus_transition(
        &mut self,
        session_id: &str,
        from: Option<String>,
        to: Option<String>,
    ) {
        self.graphs.record_transition(session_id, from, to);
    }

    /// The focus-transition ledger.
    pub fn focus_transitions(&self) -> &[(u64, String, Option<String>, Option<String>)] {
        &self.graphs.focus_transitions
    }

    /// The run's graphs (focus history + exploration), for readers that
    /// fold over them directly. Round-2 (G1): delegates to
    /// `graph_state::RunGraphs`.
    pub fn graphs(&self) -> &graph_state::RunGraphs {
        &self.graphs
    }

    /// Mutable access to the run's graphs (audit drivers, exploration).
    pub fn graphs_mut(&mut self) -> &mut graph_state::RunGraphs {
        &mut self.graphs
    }

    /// Record one focus transition into BOTH focus ledgers (Wave D item
    /// 36): the legacy label list for display continuity, and the
    /// control-ID [`crate::semantic::focus_graph::FocusGraph`] for proof.
    /// `via` names the input that produced the transition; the graph only
    /// joins transitions whose ends carry stable control IDs — label-only
    /// observations stay in the legacy ledger.
    pub fn record_focus_observation(
        &mut self,
        session_id: &str,
        from_label: Option<String>,
        to_label: Option<String>,
        from_id: Option<&str>,
        to_id: Option<&str>,
        via: &str,
    ) {
        self.graphs
            .record_observation(session_id, from_label, to_label, from_id, to_id, via);
    }

    /// Assign the next citable frame id (Wave B item 11) and stamp the
    /// frame with run provenance. Returns the id (`frame:N`).
    pub fn register_frame(&mut self, frame: &mut crate::backend::CanonicalFrame) -> u64 {
        let id = self.evidence.frames.allocate_id();
        frame.assign_frame_id(id);
        frame.run_id = Some(self.id().to_string());
        id
    }

    /// Recall one committed frame's HOT record by citable id (item 51).
    /// `None` means not in the hot ring — either never committed to this
    /// run, or evicted (check [`Self::frame_hot_evicted`]); the cold log
    /// (`frames.jsonl`, persistent runs) still resolves it.
    pub fn frame_record(&self, frame_id: u64) -> Option<&FrameRecord> {
        self.evidence.frames.record(frame_id)
    }

    /// The hot frame ring, oldest first (item 51 storage split: hot ids +
    /// hashes here, full grids only in the cold log).
    pub fn frame_hot_records(&self) -> impl Iterator<Item = &FrameRecord> {
        self.evidence.frames.hot()
    }

    /// How many hot records have been evicted past the ring bound.
    pub fn frame_hot_evicted(&self) -> u64 {
        self.evidence.frames.evicted()
    }

    /// The frame commit pipeline (re-review item 40): the ONE path a
    /// captured frame takes from raw capture to citable evidence.
    ///
    /// ```text
    /// capture ──▶ assign frame_id ──▶ stamp run/session provenance
    ///         ──▶ persist to frames.jsonl (persistent runs, O_APPEND)
    /// ```
    ///
    /// Returns the citable id. Persistence is incremental (one append per
    /// committed frame) so a crash keeps every frame committed before it;
    /// a write failure marks the run `persistence_unhealthy` and propagates
    /// as `Err` after the frame is registered in memory.
    ///
    /// Audit P0 (beta stability): the error is visible on both channels —
    /// the returned `Err` names the frame id and the durable append
    /// failure, so callers can report `memory_committed` (the id exists,
    /// the ring holds it) distinctly from `durably_committed` (frames.jsonl
    /// holds it). The old code marked the store unhealthy but returned
    /// `Ok(id)`, making a generated id indistinguishable from proof of
    /// persistence.
    pub fn commit_frame(
        &mut self,
        frame: &mut crate::backend::CanonicalFrame,
        session: Option<&str>,
    ) -> anyhow::Result<u64> {
        self.ensure_open()?;
        let started = std::time::Instant::now();
        let id = self.register_frame(frame);
        if frame.session_id.is_none() {
            frame.session_id = session.map(str::to_string);
        }
        // HOT half of the storage split (item 51): the projection facts
        // land in the bounded ring even for ephemeral runs — a citable
        // frame id that answers no questions would be a number, not
        // evidence. The semantic identity is the ESTABLISHED fused truth when
        // the capture stamped it (review P0.5) — evidence persistence
        // serializes truth and never recomputes it — falling back to the bare
        // grid identity only for frames captured before fused stamping.
        let semantic_identity = frame
            .semantic_identity
            .clone()
            .unwrap_or_else(|| crate::semantic::semantic_identity(&frame.state));
        let record = FrameRecord {
            frame_id: id,
            session: frame.session_id.clone(),
            generation: frame.generation,
            screen_seq: frame.screen_seq,
            output_seq: frame.output_seq,
            structure_hash: frame.state.structure_hash.clone(),
            visual_hash: frame.state.visual_hash.clone(),
            semantic_identity,
            committed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            commit_us: started.elapsed().as_micros() as u64,
        };
        self.evidence.frames.push_hot(record);
        if let Some(dir) = self.run_dir.as_ref() {
            let line = serde_json::json!({
                "frame_id": id,
                "run_id": frame.run_id,
                "session": frame.session_id,
                "generation": frame.generation,
                "screen_seq": frame.screen_seq,
                "output_seq": frame.output_seq,
                "structure_hash": frame.state.structure_hash,
                "visual_hash": frame.state.visual_hash,
                "semantic_identity": frame.semantic_identity.clone().unwrap_or_else(|| crate::semantic::semantic_identity(&frame.state)),
            });
            let path = dir.join("frames.jsonl");
            // Finding 31: a NEW stream file starts with its format header.
            let is_new = std::fs::metadata(&path)
                .map(|m| m.len() == 0)
                .unwrap_or(true);
            let body = if is_new {
                format!(
                    "{}\n{line}\n",
                    StreamHeader::line(crate::run::formats::tags::FRAMES)
                )
            } else {
                format!("{line}\n")
            };
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()))
            {
                Ok(()) => {}
                Err(e) => {
                    self.artifacts_store.mark_unhealthy();
                    return Err(anyhow::anyhow!(e).context(format!(
                        "durable frame append failed for frame {id} (committed in memory; run persistence is unhealthy)"
                    )));
                }
            }
        }
        Ok(id)
    }

    /// Hold one session's drained terminal events for persistence (Wave B
    /// item 14). Sessions own the live queue; the run keeps the export.
    ///
    /// Persistent runs (re-review item 38) append each batch to
    /// `events/<session>.jsonl` *immediately* — one O_APPEND write per
    /// batch — so a crash loses at most the batch in flight, never the
    /// whole session history. `flush` then only handles the ephemeral-run
    /// backlog (whole-file write at close, the old contract).
    pub fn hold_events(
        &mut self,
        session: &str,
        events: Vec<crate::events::TerminalEvent>,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        if events.is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.run_dir.as_ref() {
            let ev_dir = dir.join("events");
            if std::fs::create_dir_all(&ev_dir).is_err() {
                self.artifacts_store.mark_unhealthy();
                self.evidence.events.hold(session, events);
                return Ok(());
            }
            let safe = sanitize(session);
            let path = ev_dir.join(format!("{}.jsonl", safe));
            // Finding 31: a NEW session log starts with its format header.
            let is_new = std::fs::metadata(&path)
                .map(|m| m.len() == 0)
                .unwrap_or(true);
            let mut body = String::new();
            if is_new {
                body.push_str(&StreamHeader::line(crate::run::formats::tags::EVENTS));
                body.push('\n');
            }
            for ev in &events {
                if let Ok(line) = serde_json::to_string(ev) {
                    body.push_str(&line);
                    body.push('\n');
                }
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()))
            {
                Ok(()) => {
                    self.evidence.events.note_flushed(session, events.len());
                    // Artifact registered once per session (the log is a
                    // growing file, not a per-batch artifact).
                    let artifact_path =
                        std::path::PathBuf::from("events").join(format!("{}.jsonl", safe));
                    if !self
                        .artifacts_store
                        .artifacts()
                        .iter()
                        .any(|a| a.path.as_ref() == Some(&artifact_path))
                    {
                        self.register_artifact(
                            crate::run::ArtifactKind::EventLog,
                            Some(artifact_path),
                            None,
                            Some(session.to_string()),
                            "terminal event log (incrementally persisted)",
                        )
                        .ok();
                    }
                    self.evidence.events.add_batch(events.len());
                    return Ok(());
                }
                Err(_) => {
                    self.artifacts_store.mark_unhealthy();
                    // Fall through to the in-memory hold so flush retries.
                }
            }
        }
        self.evidence.events.hold(session, events);
        Ok(())
    }
}
