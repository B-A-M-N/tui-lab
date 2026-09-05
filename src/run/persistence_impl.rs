//! Run persistence: create/restore/promote on-disk runs, journal flush/close.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Create an in-memory run (no artifacts written).
    pub fn ephemeral() -> Self {
        RunContext {
            id: format!("run-{}", uuid::Uuid::new_v4().simple()),
            started_at: now_ms(),
            run_dir: None,
            session_specs: HashMap::new(),
            primary_session: None,
            dropped_records: 0,
            first_available_seq: None,
            checkpoints: CheckpointStore::new(),
            recorders: HashMap::new(),
            saved_scenarios: HashMap::new(),
            scenario_names: HashMap::new(),
            held_recordings: Vec::new(),
            held_captures: Vec::new(),
            focus_transitions: Vec::new(),
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
            state_graph: StateGraph::new(ExplorationBudget::default()),
            findings: Vec::new(),
            transactions: Vec::new(),
            transaction_count: 0,
            event_count: 0,
            next_frame_id: 0,
            frame_hot: std::collections::VecDeque::new(),
            frame_hot_evicted: 0,
            artifacts: Vec::new(),
            ledger_flushed_upto: 0,
            journal: None,
            persistence_unhealthy: false,
            held_events: Vec::new(),
            event_flushed_counts: HashMap::new(),
            contract: None,
            contract_path: None,
            contract_baselines: HashMap::new(),
            finding_baselines: HashMap::new(),
            coverage_ledger: std::collections::BTreeMap::new(),
            event_cursors: std::collections::HashMap::new(),
            coverage_seq: 0,
            coverage_delta_cursor: 0,
            closed: false,
            resume_epoch: 0,
            restore_warnings: Vec::new(),
        }
    }

    /// Create a persistent run rooted at `.tui-lab/runs/<run-id>` under the
    /// caller-supplied `base` (re-review P0 fix 7). There is no `None`
    /// default: guessing the process cwd as an artifact destination was the
    /// exact behavior the MCP-layer policy rejects, so the lower API no
    /// longer offers it. Callers resolve the base from session cwd or an
    /// explicit request root.
    pub fn persistent(base: &std::path::Path) -> anyhow::Result<Self> {
        let mut run = RunContext::ephemeral();
        let root = Self::runs_dir_for(base, &run.id);
        std::fs::create_dir_all(root.join("checkpoints"))?;
        std::fs::create_dir_all(root.join("scenarios"))?;
        std::fs::create_dir_all(root.join("recordings"))?;
        std::fs::create_dir_all(root.join("findings"))?;
        run.checkpoints =
            CheckpointStore::with_run_dir(root.join("checkpoints").to_string_lossy().to_string());
        run.run_dir = Some(root);
        run.write_manifest()?;
        Ok(run)
    }

    /// The artifact root, if this run persists.
    pub fn run_dir(&self) -> Option<&PathBuf> {
        self.run_dir.as_ref()
    }

    /// Audit P1-46: what `restore` could not bring back. Empty for a live
    /// run; a restored run carries its damage report for the rest of its
    /// life so every status/summary can say "degraded, here is what is
    /// missing".
    pub fn restore_warnings(&self) -> &[RestoreWarning] {
        &self.restore_warnings
    }

    /// The restore health block for status/resume responses: fully restored
    /// vs degraded with the warnings inline.
    pub fn restore_health(&self) -> serde_json::Value {
        let warnings = self.restore_warnings();
        serde_json::json!({
            "degraded": !warnings.is_empty(),
            "warnings": warnings,
        })
    }

    /// Wave G item 74/75 helper: resolve a run id to its directory under
    /// `base`, accepting both root shapes persist uses (`<base>/<id>` when
    /// base IS the runs dir, `<base>/.tui-lab/runs/<id>` otherwise) plus a
    /// direct `<base>/<id>` hit for callers that pass the runs dir. Returns
    /// the first directory whose manifest declares the id.
    pub fn resolve_run_dir(base: &std::path::Path, run_id: &str) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let is_runs_dir = base
            .file_name()
            .map(|f| f == std::ffi::OsStr::new("runs"))
            .unwrap_or(false);
        if is_runs_dir {
            candidates.push(base.join(run_id));
        } else {
            candidates.push(base.join(".tui-lab").join("runs").join(run_id));
            candidates.push(base.join(run_id));
        }
        for c in candidates {
            if let Ok(m) = crate::run::manifest::load(&c) {
                if m.run_id == run_id {
                    return Some(c);
                }
            }
        }
        None
    }

    /// Wave G item 75: every persisted run under `base`, newest first.
    /// A directory counts as a run when it holds a parseable `run.json`;
    /// anything else (scratch, corrupt, foreign) is named in `skipped`
    /// rather than hidden — a corrupt run is evidence, not noise.
    pub fn list_persisted(base: &std::path::Path) -> anyhow::Result<Vec<serde_json::Value>> {
        let runs_dir = base.join(".tui-lab").join("runs");
        let direct = base.file_name().map(|f| f == std::ffi::OsStr::new("runs"));
        let dir = if direct.unwrap_or(false) {
            base.to_path_buf()
        } else {
            runs_dir
        };
        let mut out: Vec<(u64, serde_json::Value)> = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // A missing runs directory means "no persisted runs yet" — an
            // empty list, not a failure (the reviewer-facing semantic is
            // "nothing here", not "the tool is broken").
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "cannot read runs directory {}: {e}",
                    dir.to_string_lossy()
                ))
            }
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let run_dir = entry.path();
            let manifest = match crate::run::manifest::load(&run_dir) {
                Ok(m) => m,
                Err(e) => {
                    out.push((
                        0,
                        json!({
                            "path": run_dir.to_string_lossy(),
                            "skipped": format!("unreadable manifest: {e}"),
                        }),
                    ));
                    continue;
                }
            };
            // Transaction ledger size on disk (the durable history — memory
            // is only a bounded window).
            let ledger = run_dir.join("transactions.jsonl");
            // A versioned stream's header line is not a transaction.
            let ledger_lines = std::fs::read_to_string(&ledger)
                .map(|s| {
                    StreamHeader::strip(&s, crate::run::formats::tags::LEDGER)
                        .ok()
                        .flatten()
                        .map(|rest| rest.lines().filter(|l| !l.trim().is_empty()).count() as u64)
                        .unwrap_or_else(|| s.lines().count() as u64)
                })
                .unwrap_or(0);
            out.push((
                manifest.started_at,
                json!({
                    "run_id": manifest.run_id,
                    "started_at": manifest.started_at,
                    "closed": manifest.closed,
                    "resume_epoch": manifest.resume_epoch,
                    "history_complete": manifest.history_complete,
                    "dropped_records": manifest.dropped_records,
                    "sessions": manifest.sessions.keys().cloned().collect::<Vec<_>>(),
                    "primary_session": manifest.primary_session,
                    "dir": run_dir.to_string_lossy(),
                    "ledger_transactions": ledger_lines,
                }),
            ));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out.into_iter().map(|(_, v)| v).collect())
    }

    /// Wave G item 74: restore a run from its durable directory. Reads the
    /// manifest, transaction ledger, findings, focus graph(s), state graph,
    /// coverage ledger, and saved scenarios back into memory and re-roots
    /// the checkpoint store — the SAME run identity continues (future
    /// interactions append to the same artifacts; `tui_run status` reports
    /// persistent again).
    ///
    /// What restore deliberately does NOT do: relaunch sessions (the
    /// manifest records their launch specs, but a restored run starts with
    /// no live sessions — `tui_session start` re-creates them and the run
    /// correlates them like any other), resurrect evicted ledger records
    /// (an evicted window is declared in the manifest and stays declared),
    /// or pretend a `closed` run is open (it stays closed until a fresh
    /// run takes over — replay/read paths still work on closed runs).
    pub fn restore(run_dir: &std::path::Path) -> anyhow::Result<Self> {
        let manifest = crate::run::manifest::load(run_dir)?;
        // Restore over the id the manifest declares — a mismatched directory
        // name is fine (identity lives in run.json), a missing id is not.
        let mut run = RunContext::ephemeral();
        run.id = manifest.run_id.clone();
        run.started_at = manifest.started_at;
        run.closed = manifest.closed;
        run.resume_epoch = manifest.resume_epoch;
        run.dropped_records = manifest.dropped_records;
        run.first_available_seq = manifest.first_available_seq;
        run.session_specs = manifest.sessions.clone();
        run.primary_session = manifest.primary_session.clone();
        run.run_dir = Some(run_dir.to_path_buf());
        run.checkpoints = CheckpointStore::with_run_dir(
            run_dir.join("checkpoints").to_string_lossy().to_string(),
        );
        // Load every persisted checkpoint back into memory (per-session
        // subdirectories). A restored run can compare against pre-close
        // snapshots again.
        if let Ok(entries) = std::fs::read_dir(run_dir.join("checkpoints")) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if let Some(sid) = entry.file_name().to_str() {
                        run.checkpoints.load_session(sid);
                    }
                }
            }
        }
        run.ledger_flushed_upto = run.transaction_count; // 0 — file is authoritative

        // Transaction ledger: every line that is still parseable comes back.
        // A torn final line (crash mid-append) is skipped and *counted*, not
        // silently dropped.
        let ledger_path = run_dir.join("transactions.jsonl");
        if let Ok(body) = std::fs::read_to_string(&ledger_path) {
            let mut torn = 0u64;
            // Finding 31: a versioned stream carries a header naming its
            // format; a mismatch is a named restore warning, not silent
            // best-effort parsing. Header-less (pre-versioning) files
            // restore as before.
            let data = match StreamHeader::strip(&body, crate::run::formats::tags::LEDGER) {
                Ok(Some(rest)) => rest,
                Ok(None) => &body[..],
                Err(e) => {
                    run.restore_warnings.push(RestoreWarning {
                        artifact: "transactions.jsonl".to_string(),
                        error: e.to_string(),
                    });
                    String::as_str(&body)
                }
            };
            for line in data.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<TransactionRecord>(line) {
                    Ok(rec) => {
                        run.transaction_count = run.transaction_count.max(rec.seq + 1);
                        run.transactions.push(rec);
                    }
                    Err(_) => torn += 1,
                }
            }
            if torn > 0 {
                run.dropped_records += torn;
                run.first_available_seq = run.transactions.first().map(|t| t.seq);
            }
            run.ledger_flushed_upto = run.transaction_count;
        }

        // Findings. Audit P1-46: a present-but-unparseable artifact is a
        // restore WARNING, not a silent skip — a forensic harness must be
        // able to see that its evidence is incomplete.
        // Helper: read + parse one JSON artifact, recording a warning on
        // either failure (present-but-unreadable / corrupt). Absent files
        // are normal (a run may never have produced one) and warn nothing.
        macro_rules! load_json {
            ($file:expr, $tag:expr, $ty:ty, $slot:expr) => {
                match std::fs::read(run_dir.join($file)) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => run.restore_warnings.push(RestoreWarning {
                        artifact: $file.to_string(),
                        error: format!("unreadable: {e}"),
                    }),
                    Ok(bytes) => {
                        // Finding 31: the artifact is versioned. An
                        // envelope is checked against the reader's tag (a
                        // mismatch is a named warning, not "corrupt"); a
                        // bare pre-envelope document parses as before.
                        match crate::run::formats::Envelope::unwrap(&bytes, $tag).and_then(|v| {
                            serde_json::from_value::<$ty>(v).map_err(anyhow::Error::from)
                        }) {
                            Ok(v) => {
                                $slot = v;
                            }
                            Err(e) => run.restore_warnings.push(RestoreWarning {
                                artifact: $file.to_string(),
                                error: format!("corrupt: {e}"),
                            }),
                        }
                    }
                }
            };
        }
        load_json!(
            "findings.json",
            crate::run::formats::tags::FINDINGS,
            Vec<crate::audit::Finding>,
            run.findings
        );
        // Focus graphs (both ledgers).
        load_json!(
            "focus_graph.json",
            crate::run::formats::tags::FOCUS_LEGACY,
            Vec<(u64, String, Option<String>, Option<String>)>,
            run.focus_transitions
        );
        load_json!(
            "focus_graph_ids.json",
            crate::run::formats::tags::FOCUS_IDS,
            crate::semantic::focus_graph::FocusGraph,
            run.focus_graph
        );
        // State graph (from its export snapshot; keeps the default budget).
        // Parsed as raw JSON, then converted — a conversion failure after a
        // successful parse is still a corrupt artifact worth naming.
        let state_graph_json: Option<serde_json::Value> = None;
        let state_graph_json = {
            let mut slot = state_graph_json;
            match std::fs::read(run_dir.join("state_graph.json")) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => run.restore_warnings.push(RestoreWarning {
                    artifact: "state_graph.json".to_string(),
                    error: format!("unreadable: {e}"),
                }),
                Ok(bytes) => {
                    // Finding 31: versioned like every other artifact.
                    match crate::run::formats::Envelope::unwrap(
                        &bytes,
                        crate::run::formats::tags::STATE_GRAPH,
                    ) {
                        Ok(v) => slot = Some(v),
                        Err(e) => run.restore_warnings.push(RestoreWarning {
                            artifact: "state_graph.json".to_string(),
                            error: format!("corrupt: {e}"),
                        }),
                    }
                }
            }
            slot
        };
        if let Some(v) = state_graph_json {
            run.state_graph = StateGraph::from_export(&v, ExplorationBudget::default());
        }
        // Coverage ledger.
        load_json!(
            "coverage.json",
            crate::run::formats::tags::COVERAGE,
            std::collections::BTreeMap<String, CoverageEntry>,
            run.coverage_ledger
        );
        // Saved scenarios from the durable dir (id-keyed; name index rebuilt).
        let scen_dir = run_dir.join("scenarios");
        if let Ok(entries) = std::fs::read_dir(&scen_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        match crate::run::formats::Envelope::unwrap(
                            &bytes,
                            crate::run::formats::tags::SCENARIO,
                        )
                        .and_then(|v| {
                            serde_json::from_value::<crate::scenario::model::Scenario>(v)
                                .map_err(anyhow::Error::from)
                        }) {
                            Ok(sc) => {
                                run.saved_scenarios.insert(sc.id.clone(), sc);
                            }
                            Err(e) => run.restore_warnings.push(RestoreWarning {
                                artifact: path
                                    .strip_prefix(run_dir)
                                    .unwrap_or(&path)
                                    .to_string_lossy()
                                    .to_string(),
                                error: format!("corrupt scenario: {e}"),
                            }),
                        }
                    }
                    Err(e) => run.restore_warnings.push(RestoreWarning {
                        artifact: path
                            .strip_prefix(run_dir)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .to_string(),
                        error: format!("unreadable scenario: {e}"),
                    }),
                }
            }
        }
        run.rebuild_scenario_name_index();
        // Findings baselines: not persisted as a file (they are a live-run
        // working set); a restored run starts without them and `tui_audit
        // label=X` can rebuild one in one call. Same for contract baselines
        // and the loaded contract itself (contract_path survives in the
        // manifest? no — the run dir carries no contract copy; the caller
        // re-loads with tui_contract action=load).

        // Artifacts: re-register what is actually on disk so references
        // survive restore.
        fn reg(
            run: &mut RunContext,
            n: usize,
            kind: crate::run::ArtifactKind,
            rel: std::path::PathBuf,
            summary: String,
        ) {
            run.artifacts.push(crate::run::ArtifactRef {
                id: format!("art-{}", n),
                kind,
                path: Some(rel.clone()),
                size: None,
                session: None,
                summary: format!("{summary} ({})", rel.to_string_lossy()),
            });
        }
        fn scan_dir(
            run: &mut RunContext,
            run_dir: &std::path::Path,
            n: &mut usize,
            sub: &str,
            kind: crate::run::ArtifactKind,
            summary: &str,
        ) {
            let dir = run_dir.join(sub);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
                files.sort();
                for f in files {
                    if f.is_file() {
                        *n += 1;
                        let rel = std::path::PathBuf::from(sub).join(
                            f.file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        );
                        reg(run, *n, kind.clone(), rel, summary.to_string());
                    }
                }
            }
        }
        let mut n = 0usize;
        scan_dir(
            &mut run,
            run_dir,
            &mut n,
            "recordings",
            crate::run::ArtifactKind::Recording,
            "pty recording (restored run)",
        );
        scan_dir(
            &mut run,
            run_dir,
            &mut n,
            "captures",
            crate::run::ArtifactKind::Capture,
            "screen capture (restored run)",
        );
        scan_dir(
            &mut run,
            run_dir,
            &mut n,
            "events",
            crate::run::ArtifactKind::EventLog,
            "terminal event log (restored run)",
        );

        Ok(run)
    }

    /// Re-open a closed run so it accepts driving and evidence again. This
    /// is the `tui_run resume` path: resuming a persisted run is the
    /// designated way to make it live once more (review P0.1). A persisted
    /// run that was closed on disk stays durable.
    ///
    /// Finding 32: reopen is not invisible. Each reopen bumps the run's
    /// `resume_epoch` — 0 for the original process's run, 1 after the first
    /// resume, and so on — and the manifest is written BEFORE anything else
    /// runs, so a reader can always tell which epoch a record belongs to.
    /// A reopened run is NOT the original process's run: its later records
    /// live after a process boundary it did not choose, and evidence taken
    /// across that boundary is distinguishable via the epoch.
    pub fn reopen(&mut self) {
        self.resume_epoch += 1;
        self.closed = false;
        // Persist the epoch before returning: if the resumed process dies
        // mid-transaction, the manifest still names the epoch everything
        // after it belongs to.
        if let Err(e) = self.write_manifest() {
            tracing::warn!(run = %self.id, error = %e, "reopen: manifest write failed");
        }
    }

    /// Mark the run closed and flush durable state. Does NOT touch sessions —
    /// killing them is the caller's explicit decision.
    pub fn close(&mut self) -> anyhow::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        // Flush everything flushable to the artifact root (no-op when
        // ephemeral — ephemeral means "not persisted", not "degraded").
        self.flush()?;
        self.write_manifest()
    }

    /// Flush accumulated in-memory run state to the artifact root: saved
    /// scenarios not yet on disk, the state graph, and findings.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        let Some(dir) = self.run_dir.clone() else {
            return Ok(());
        };
        std::fs::create_dir_all(&dir)?;
        // Scenarios held only in memory. File names match save_scenario's
        // `<name>-<id-suffix>.json` shape (Wave-1 item 9).
        let scen_dir = dir.join("scenarios");
        std::fs::create_dir_all(&scen_dir)?;
        let mut pending: Vec<String> = self.saved_scenarios.keys().cloned().collect();
        pending.sort();
        for id in pending {
            if let Some(sc) = self.saved_scenarios.get(&id) {
                let short_id = sc.id.rsplit('-').next().unwrap_or("0");
                let stem = format!("{}-{}", sanitize(&sc.name), short_id);
                let path = scen_dir.join(format!("{}.json", stem));
                if path.exists() {
                    continue;
                }
                let tmp = scen_dir.join(format!("{}.json.tmp", stem));
                std::fs::write(
                    &tmp,
                    format_envelope(
                        crate::run::formats::tags::SCENARIO,
                        &serde_json::to_value(sc)?,
                    )?,
                )?;
                std::fs::rename(&tmp, &path)?;
            }
        }
        // State graph.
        let graph = self.state_graph.export();
        let tmp = dir.join("state_graph.json.tmp");
        std::fs::write(
            &tmp,
            format_envelope(crate::run::formats::tags::STATE_GRAPH, &graph)?,
        )?;
        std::fs::rename(&tmp, dir.join("state_graph.json"))?;
        // Focus graph (legacy label ledger + the ID-keyed FocusGraph).
        let ftmp = dir.join("focus_graph.json.tmp");
        std::fs::write(
            &ftmp,
            format_envelope(
                crate::run::formats::tags::FOCUS_LEGACY,
                &serde_json::to_value(&self.focus_transitions)?,
            )?,
        )?;
        std::fs::rename(&ftmp, dir.join("focus_graph.json"))?;
        let gtmp = dir.join("focus_graph_ids.json.tmp");
        std::fs::write(
            &gtmp,
            format_envelope(
                crate::run::formats::tags::FOCUS_IDS,
                &serde_json::to_value(&self.focus_graph)?,
            )?,
        )?;
        std::fs::rename(&gtmp, dir.join("focus_graph_ids.json"))?;
        // Transaction ledger (Wave-2 item 15 + Wave B item 14 + the
        // run-journal-writer audit item): records stream to the background
        // writer as they are recorded; flush DRAINS — it waits for the
        // writer's watermark to reach the total record count (bounded
        // wait, honest timeout) so a crash loses at most the in-flight
        // channel tail, and `flush` never reports success while lines are
        // still unwritten.
        if let Some(j) = &self.journal {
            let target = self.transaction_count;
            let reached = j.wait_for(target, std::time::Duration::from_secs(5));
            if reached < target || j.is_unhealthy() {
                self.persistence_unhealthy = true;
            }
            self.ledger_flushed_upto = reached;
        } else if self.transaction_count > self.ledger_flushed_upto {
            // Restored run with no writer yet (no new records since
            // restore): the file is already authoritative; nothing to do.
            self.ledger_flushed_upto = self.transaction_count;
        }
        // Terminal-event logs (Wave B item 12/14).
        let ev_dir = dir.join("events");
        std::fs::create_dir_all(&ev_dir)?;
        for (session, events) in std::mem::take(&mut self.held_events) {
            let safe = sanitize(&session);
            // Finding 31: this whole-file write is the stream CREATOR for
            // ephemeral runs flushed at close — it carries the header.
            let mut body = String::new();
            body.push_str(&StreamHeader::line(crate::run::formats::tags::EVENTS));
            body.push('\n');
            for ev in &events {
                body.push_str(&serde_json::to_string(ev)?);
                body.push('\n');
            }
            let path = ev_dir.join(format!("{}.jsonl", safe));
            let tmp = ev_dir.join(format!("{}.jsonl.tmp", safe));
            std::fs::write(&tmp, &body)?;
            std::fs::rename(&tmp, &path)?;
            self.register_artifact(
                crate::run::ArtifactKind::EventLog,
                Some(std::path::PathBuf::from("events").join(format!("{}.jsonl", safe))),
                Some(body.len() as u64),
                Some(session),
                format!("terminal event log, {} events", events.len()),
            )?;
        }
        // Recordings held in memory (stopped while ephemeral).
        let rec_dir = dir.join("recordings");
        std::fs::create_dir_all(&rec_dir)?;
        let mut still_held = Vec::new();
        for (name, body) in std::mem::take(&mut self.held_recordings) {
            // Names come from the recorder (session id + millis): already
            // path-safe, so preserve them verbatim rather than re-sanitizing
            // (which would rewrite '.' to '_').
            match std::fs::write(rec_dir.join(&name), &body) {
                Ok(()) => {}
                Err(_) => still_held.push((name, body)),
            }
        }
        self.held_recordings = still_held;
        // Wave F item 57: screen captures held while ephemeral.
        let cap_dir = dir.join("captures");
        std::fs::create_dir_all(&cap_dir)?;
        let mut still_held_caps = Vec::new();
        for (name, body, format) in std::mem::take(&mut self.held_captures) {
            match std::fs::write(cap_dir.join(&name), &body) {
                Ok(()) => {
                    self.register_artifact(
                        crate::run::ArtifactKind::Capture,
                        Some(std::path::PathBuf::from("captures").join(&name)),
                        Some(body.len() as u64),
                        None,
                        format!("screen capture ({format} format)"),
                    )?;
                }
                Err(_) => still_held_caps.push((name, body, format)),
            }
        }
        self.held_captures = still_held_caps;
        // Findings.
        let fdir = dir.join("findings");
        std::fs::create_dir_all(&fdir)?;
        let ftmp = dir.join("findings.json.tmp");
        std::fs::write(
            &ftmp,
            format_envelope(
                crate::run::formats::tags::FINDINGS,
                &serde_json::to_value(&self.findings)?,
            )?,
        )?;
        std::fs::rename(&ftmp, dir.join("findings.json"))?;
        // Wave F item 64: the native coverage ledger.
        if !self.coverage_ledger.is_empty() {
            let ctmp = dir.join("coverage.json.tmp");
            std::fs::write(
                &ctmp,
                format_envelope(
                    crate::run::formats::tags::COVERAGE,
                    &serde_json::to_value(&self.coverage_ledger)?,
                )?,
            )?;
            std::fs::rename(&ctmp, dir.join("coverage.json"))?;
        }
        Ok(())
    }

    /// Promote this SAME run from ephemeral to persistent (goal spec:
    /// identity + all accumulated state preserved; future writes go to disk).
    ///
    /// `base` is resolved by the caller from the primary session's
    /// `LaunchSpec.cwd` or an explicit request root — never from this
    /// process's cwd.
    pub fn promote(&mut self, base: &std::path::Path) -> anyhow::Result<PathBuf> {
        if let Some(existing) = &self.run_dir {
            return Ok(existing.clone());
        }
        let root = Self::runs_dir_for(base, &self.id);
        std::fs::create_dir_all(root.join("checkpoints"))?;
        std::fs::create_dir_all(root.join("scenarios"))?;
        std::fs::create_dir_all(root.join("recordings"))?;
        std::fs::create_dir_all(root.join("findings"))?;
        // Same store object, re-rooted: in-memory checkpoints survive
        // promotion and are flushed into the new durable root.
        self.checkpoints
            .reroot(root.join("checkpoints").to_string_lossy().to_string());
        self.run_dir = Some(root.clone());
        // Records accumulated while ephemeral never went through the
        // journal writer — write the whole window now (one synchronous
        // pass at promotion, not per-record on the driving path), then
        // arm the writer for everything after this point.
        if self.transaction_count > 0 {
            let ledger_path = root.join("transactions.jsonl");
            // Finding 31: promotion CREATES the stream file — it carries
            // the format header before the records.
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&ledger_path)
            {
                use std::io::Write as _;
                let _ = file
                    .write_all(StreamHeader::line(crate::run::formats::tags::LEDGER).as_bytes());
                let _ = file.write_all(b"\n");
                for tx in &self.transactions {
                    if let Ok(line) = serde_json::to_string(tx) {
                        let _ = file.write_all(line.as_bytes());
                        let _ = file.write_all(b"\n");
                    }
                }
            }
            self.ledger_flushed_upto = self.transaction_count;
        }
        // Flush everything accumulated while ephemeral into the new root.
        self.flush()?;
        self.write_manifest()?;
        Ok(root)
    }

    /// Wait for the background journal writer to drain (bounded). Returns
    /// the watermark reached. A no-op for ephemeral runs.
    pub fn wait_for_journal(&self, deadline: std::time::Duration) -> u64 {
        match &self.journal {
            Some(j) => j.wait_for(self.transaction_count, deadline),
            None => self.transaction_count,
        }
    }

    /// Whether ledger persistence has failed and the run is degrading to
    /// memory-only (writer spawn error or terminal write error).
    pub fn persistence_unhealthy(&self) -> bool {
        self.persistence_unhealthy || self.journal.as_ref().is_some_and(|j| j.is_unhealthy())
    }
}
