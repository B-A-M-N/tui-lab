//! The canonical run/artifact model (audit item: "Run/Artifact Model").
//!
//! Every longer-lived activity of the lab — exploration, audits, scenarios,
//! recordings, coverage — happens inside a *run* and leaves its artifacts in
//! one place:
//!
//! ```text
//! .tui-lab/runs/<run-id>/
//!     run.json            — RunManifest (serialized RunContext identity)
//!     checkpoints/        — CheckpointStore persistence
//!     scenarios/          — recorded + saved scenarios
//!     recordings/         — asciicast exports
//!     findings/           — audit findings
//!     state_graph.json    — exploration graph export
//! ```
//!
//! `RunContext` is the composition root: instead of the MCP layer owning
//! disconnected global maps (`CHECKPOINTS`, `SCENARIOS`, `write_cast`), the
//! server holds a `RunContext` and every subsystem hangs off it. This is what
//! turns "implemented modules" into "product paths".

pub mod artifacts;
pub mod manifest;
pub mod recording_scope;

pub use artifacts::{ArtifactKind, ArtifactRef};
pub use manifest::RunManifest;

use crate::checkpoint::store::CheckpointStore;
use crate::exploration::state_graph::{ExplorationBudget, StateGraph};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bound on the in-memory transaction ledger (re-review Wave-2 item
/// 15). Frames are heavy; the ledger holds evidence-level records. When the
/// bound is hit the oldest half is evicted; the eviction is *declared* —
/// `dropped_records`/`first_available_seq` travel with the run (manifest +
/// status) so a promoted run never pretends to be replay-complete
/// (re-review P0 fix 5).
const MAX_TRANSACTION_RECORDS: usize = 512;

/// One settled interaction transaction, at evidence level.
///
/// This is the reconstructable record the run previously lacked (it could
/// say `transactions: 47` but not replay any of them): ordered sequence,
/// action, settle outcome, before/after hashes, and the cell delta. Full
/// frames remain the caller's concern — attach them to artifacts, not to
/// every run entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionRecord {
    /// Ordered execution index within the run (0-based).
    pub seq: u64,
    /// Unix-millis timestamp of the transaction.
    pub at: u64,
    /// Session the transaction ran against (informational; the run does not
    /// own sessions).
    pub session: String,
    /// Action name ("key", "type", "mouse_click", "wait", ...).
    pub action: String,
    /// How the settle wait resolved ("met" / "timed_out" / "skipped").
    /// Serialized as a string; `settled()` derives the legacy bool.
    pub settle: String,
    /// Why the settle resolved (or timed out / was skipped), if reported.
    pub settle_reason: Option<String>,
    /// Structure hash before the action.
    pub before_structure: String,
    /// Structure hash after the action (the matching/settled frame).
    pub after_structure: String,
    /// Cells changed across the transition.
    pub changed_cells: usize,
    /// Settle latency in milliseconds.
    pub elapsed_ms: u64,
    /// The typed action (Wave-2 item 10) as it may be persisted (leak fix):
    /// `Full` when the visibility policy allows it, `Redacted { kind,
    /// byte_len }` for sensitive payloads — the payload itself never lands
    /// in the ledger. Skipped for non-interaction ledger entries ("wait").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_action: Option<crate::execution::PersistedAction>,
}

impl TransactionRecord {
    /// Legacy view of settlement as a bool.
    pub fn settled(&self) -> bool {
        self.settle == "met"
    }

    /// Build a record from a live [`crate::execution::InteractionTransaction`].
    pub fn from_interaction(
        seq: u64,
        session: &str,
        tx: &crate::execution::InteractionTransaction,
    ) -> Self {
        let settle = match tx.settle {
            crate::execution::SettleStatus::Met => "met",
            crate::execution::SettleStatus::TimedOut => "timed_out",
            crate::execution::SettleStatus::Skipped => "skipped",
        };
        let persisted_action = Some(crate::execution::PersistedAction::project(
            tx.canonical(),
            tx.action.visibility,
        ));
        TransactionRecord {
            seq,
            at: now_ms(),
            session: session.to_string(),
            action: tx.name().to_string(),
            settle: settle.to_string(),
            settle_reason: Some(tx.settle_reason()),
            before_structure: tx.before().structure_hash.clone(),
            after_structure: tx.after().structure_hash.clone(),
            changed_cells: tx.transition.screen_diff.changed_cells,
            elapsed_ms: tx.elapsed_ms,
            persisted_action,
        }
    }
}

/// Identity + artifact directory for one run.
pub struct RunContext {
    /// Opaque run id (`run-<uuid simple>`).
    pub id: String,
    /// Wall-clock start (unix millis).
    pub started_at: u64,
    /// Artifact root: `.tui-lab/runs/<id>` when persistence is enabled.
    run_dir: Option<PathBuf>,
    /// Launch spec per session (re-review P0 fix 6): a run can involve
    /// multiple sessions and each restart creates a new generation, so the
    /// run records one entry per session id it has seen.
    session_specs: HashMap<String, crate::session::state::LaunchSpec>,
    /// First session recorded — the run's primary for cwd/root resolution.
    primary_session: Option<String>,
    /// How many ledger records were evicted before flush (P0 fix 5): when
    /// nonzero, the durable run is *not* replay-complete and says so.
    dropped_records: u64,
    /// Sequence number of the oldest record still in the ledger; `None`
    /// when nothing was evicted (equivalent to 0).
    first_available_seq: Option<u64>,
    /// Checkpoints recorded during this run.
    pub checkpoints: CheckpointStore,
    /// In-progress scenario recordings by opaque id (re-review item 5:
    /// session+generation scoped; names are not identities).
    recorders: HashMap<String, recording_scope::ScenarioRecording>,
    /// Completed scenarios for this run, keyed by [`Scenario::id`] (re-review
    /// Wave-1 item 9: names are display metadata and may collide; ids are the
    /// storage key). Memory is canonical during the live run (so ephemeral
    /// runs still list/export); persistence to the run dir is an artifact
    /// layer on top, not the source of truth. The secondary name index only
    /// maps *unambiguous* names — a name held by two scenarios resolves
    /// through neither.
    saved_scenarios: HashMap<String, crate::scenario::model::Scenario>,
    scenario_names: HashMap<String, String>,
    /// Completed PTY recordings retained in memory (run promotion must carry
    /// them into the durable root even when they were stopped while
    /// ephemeral). Keyed by suggested file name.
    held_recordings: Vec<(String, String)>,
    /// Wave F item 57: screen captures held while the run is ephemeral
    /// (name, bytes, format).
    held_captures: Vec<(String, Vec<u8>, String)>,
    /// Focus-transition ledger: (unix_ms, session, from, to) recorded from
    /// semantic analysis of every observation. The run's focus graph.
    focus_transitions: Vec<(u64, String, Option<String>, Option<String>)>,
    /// The real FocusGraph (Wave D items 36–37): focus transitions as edges
    /// keyed on stable control IDs with the input that produced them, so Tab
    /// order and Shift+Tab reversal are provable, not suggested. Recorded
    /// from the same observations as `focus_transitions` (labels) plus the
    /// audit drivers (driven edges).
    pub focus_graph: crate::semantic::focus_graph::FocusGraph,
    /// Exploration state graph (owned here so audits/exploration share it).
    pub state_graph: StateGraph,
    /// Findings emitted during this run (audit results).
    findings: Vec<crate::audit::Finding>,
    /// Transaction ledger (re-review Wave-2 item 15): a serializable record
    /// of every interaction transaction, so the run can *reconstruct* what
    /// happened (`transactions: 47` without 47 reconstructable transactions
    /// was the old shape). Bounded to MAX_TRANSACTION_RECORDS; full frames
    /// stay in the callers' hands — this is the evidence-level record.
    transactions: Vec<TransactionRecord>,
    /// Interaction transactions executed in this run (act/wait counters).
    transaction_count: u64,
    /// Events observed in this run (observe/wait calls).
    event_count: u64,
    /// Per-run frame id allocator (Wave B item 11): every CanonicalFrame
    /// registered with the run gets a citable `frame:N` identity.
    next_frame_id: u64,
    /// Artifact registry (Wave B item 15): typed references to every large
    /// artifact the run produced (recordings, event logs). Tools return an
    /// [`ArtifactRef`] instead of inlining multi-kilobyte payloads.
    artifacts: Vec<ArtifactRef>,
    /// True once the ledger has been appended to transactions.jsonl for a
    /// persistent run (drives incremental appends, Wave B item 14).
    ledger_flushed_upto: u64,
    /// Terminal-event batches drained from sessions (Wave B item 12/14):
    /// (session id, events). Filled by the MCP layer before flush; written
    /// to `events/<session>.jsonl` when the run persists.
    held_events: Vec<(String, Vec<crate::events::TerminalEvent>)>,
    /// The loaded project contract (Wave E item 39): feeds conformance
    /// checks, exploration candidates, and audits.
    contract: Option<crate::design::ProjectContract>,
    /// Where that contract was loaded from (display/evidence).
    contract_path: Option<String>,
    /// Conformance baselines for `compare` (Wave E item 47): label → the
    /// report captured under that label.
    contract_baselines: HashMap<String, crate::design::ContractReport>,
    /// Wave G item 67: labeled audit-finding baselines for FIXED/REGRESSED/
    /// NEW comparison. `tui_audit label=X` stores this pass's findings under
    /// X; `tui_audit compare_to=X` diffs the fresh pass against the stored
    /// one via finding fingerprints.
    finding_baselines: HashMap<String, Vec<crate::audit::Finding>>,
    /// Wave F item 64: native coverage ledger. Entries accumulate from
    /// NativeSemanticProtocol `coverage` events: target → (hits, sessions).
    /// This is interaction-correlated coverage: "did my last act exercise
    /// new app code" is answerable by diffing before/after a step.
    pub coverage_ledger: std::collections::BTreeMap<String, CoverageEntry>,
    /// Set by `tui_run close`. Sessions are NOT touched by closing.
    closed: bool,
}

/// One native coverage entry (Wave F item 64): an app-declared coverage
/// target (file:line, widget id, function name) plus its hit statistics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CoverageEntry {
    /// Total hits declared by the app.
    pub hits: u64,
    /// Sessions that reported this target.
    pub sessions: Vec<String>,
    /// Unix-millis of the first report.
    pub first_seen: u64,
    /// Unix-millis of the last report.
    pub last_seen: u64,
}

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
            artifacts: Vec::new(),
            ledger_flushed_upto: 0,
            held_events: Vec::new(),
            contract: None,
            contract_path: None,
            contract_baselines: HashMap::new(),
            finding_baselines: HashMap::new(),
            coverage_ledger: std::collections::BTreeMap::new(),
            closed: false,
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
            let ledger_lines = std::fs::read_to_string(&ledger)
                .map(|s| s.lines().count() as u64)
                .unwrap_or(0);
            out.push((
                manifest.started_at,
                json!({
                    "run_id": manifest.run_id,
                    "started_at": manifest.started_at,
                    "closed": manifest.closed,
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
            for line in body.lines() {
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

        // Findings.
        if let Ok(bytes) = std::fs::read(run_dir.join("findings.json")) {
            if let Ok(f) = serde_json::from_slice::<Vec<crate::audit::Finding>>(&bytes) {
                run.findings = f;
            }
        }
        // Focus graphs (both ledgers).
        if let Ok(bytes) = std::fs::read(run_dir.join("focus_graph.json")) {
            if let Ok(t) =
                serde_json::from_slice::<Vec<(u64, String, Option<String>, Option<String>)>>(&bytes)
            {
                run.focus_transitions = t;
            }
        }
        if let Ok(bytes) = std::fs::read(run_dir.join("focus_graph_ids.json")) {
            if let Ok(g) =
                serde_json::from_slice::<crate::semantic::focus_graph::FocusGraph>(&bytes)
            {
                run.focus_graph = g;
            }
        }
        // State graph (from its export snapshot; keeps the default budget).
        if let Ok(bytes) = std::fs::read(run_dir.join("state_graph.json")) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                run.state_graph = StateGraph::from_export(&v, ExplorationBudget::default());
            }
        }
        // Coverage ledger.
        if let Ok(bytes) = std::fs::read(run_dir.join("coverage.json")) {
            if let Ok(l) =
                serde_json::from_slice::<std::collections::BTreeMap<String, CoverageEntry>>(&bytes)
            {
                run.coverage_ledger = l;
            }
        }
        // Saved scenarios from the durable dir (id-keyed; name index rebuilt).
        let scen_dir = run_dir.join("scenarios");
        if let Ok(entries) = std::fs::read_dir(&scen_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(sc) =
                        serde_json::from_slice::<crate::scenario::model::Scenario>(&bytes)
                    {
                        run.saved_scenarios.insert(sc.id.clone(), sc);
                    }
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

    /// The loaded project contract, if any. Feeds exploration candidates
    /// (item 49) and `tui_contract status/compare`.
    pub fn contract(&self) -> Option<&crate::design::ProjectContract> {
        self.contract.as_ref()
    }

    /// Record the loaded contract (replaces any previous one — the newest
    /// contract wins, matching the tool's load semantics).
    pub fn set_contract(&mut self, contract: crate::design::ProjectContract, path: String) {
        self.contract = Some(contract);
        self.contract_path = Some(path);
        self.write_manifest().ok();
    }

    /// Where the current contract was loaded from.
    pub fn contract_path(&self) -> Option<&str> {
        self.contract_path.as_deref()
    }

    /// Named conformance baselines for `tui_contract compare`: label →
    /// report. `status` writes "baseline" (the first trusted state);
    /// `compare` writes the label it was given so later comparisons have
    /// history.
    pub fn contract_baselines(&self) -> &HashMap<String, crate::design::ContractReport> {
        &self.contract_baselines
    }

    /// Store (or overwrite) a labeled audit-finding baseline (item 67).
    pub fn record_finding_baseline(&mut self, label: &str, findings: Vec<crate::audit::Finding>) {
        self.finding_baselines.insert(label.to_string(), findings);
    }

    /// Fetch a labeled audit-finding baseline.
    pub fn finding_baseline(&self, label: &str) -> Option<&Vec<crate::audit::Finding>> {
        self.finding_baselines.get(label)
    }

    /// Labels of all stored finding baselines (for honest "no such label"
    /// errors that name what exists).
    pub fn finding_baseline_labels(&self) -> Vec<String> {
        let mut l: Vec<String> = self.finding_baselines.keys().cloned().collect();
        l.sort();
        l
    }

    pub fn record_contract_baseline(
        &mut self,
        label: &str,
        report: &crate::design::ContractReport,
    ) {
        self.contract_baselines
            .insert(label.to_string(), report.clone());
    }

    /// Start recording a scenario bound to one session generation. Returns
    /// the recording id. Names are display labels, not identities — parallel
    /// recordings may share a name.
    pub fn begin_scenario_recording(
        &mut self,
        name: &str,
        session_id: &str,
        generation: u32,
    ) -> recording_scope::ScenarioRecordingId {
        let rec = recording_scope::ScenarioRecording::new(name, session_id, generation);
        let id = rec.id.clone();
        self.recorders.insert(id.as_str().to_string(), rec);
        id
    }

    /// Record an act step into every recording scoped to this exact session
    /// generation (re-review item 5: session A's recording never absorbs
    /// session B's traffic).
    pub fn record_scenario_act(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) {
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_act(params.clone());
            }
        }
    }

    /// Record a wait step into matching session-generation recordings.
    pub fn record_scenario_wait(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) {
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_wait(params.clone());
            }
        }
    }

    /// Record an assert step into matching session-generation recordings.
    pub fn record_scenario_assert(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) {
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_assert(params.clone());
            }
        }
    }

    /// Finish a recording by id: returns the completed scenario and stops
    /// tracking it.
    pub fn finish_scenario_recording(
        &mut self,
        recording_id: &str,
    ) -> Option<crate::scenario::model::Scenario> {
        let mut rec = self.recorders.remove(recording_id)?;
        Some(rec.finish())
    }

    /// Look up a recording id by name (any session). Ambiguous names return
    /// the oldest match — callers that care about identity should hold ids
    /// from `begin_scenario_recording`.
    pub fn find_recording_by_name(&self, name: &str) -> Option<String> {
        self.recorders
            .values()
            .filter(|r| r.name == name)
            .min_by_key(|r| r.started_at)
            .map(|r| r.id.as_str().to_string())
    }

    /// Metadata for all active recordings.
    pub fn active_recordings(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = self
            .recorders
            .values()
            .map(|r| {
                serde_json::json!({
                    "id": r.id.as_str(),
                    "name": r.name,
                    "session": r.session_id,
                    "generation": r.generation,
                    "steps": r.recorder.step_count_hint(),
                })
            })
            .collect();
        out.sort_by_key(|v| v["id"].as_str().unwrap_or("").to_string());
        out
    }

    /// True while any recording is active for this exact session generation.
    pub fn is_recording_scenario(&self, session_id: &str, generation: u32) -> bool {
        self.recorders
            .values()
            .any(|r| r.matches(session_id, generation))
    }

    /// Save a scenario: memory first (canonical), then disk when the run is
    /// persistent. Storage key is the scenario's *id* (re-review Wave-1 item
    /// 9); same-named scenarios from different sessions coexist. Returns the
    /// artifact path when persistence happened, so ephemeral runs report
    /// `saved_to: null` honestly but the scenario is still listed and
    /// exportable.
    pub fn save_scenario(&mut self, scenario: crate::scenario::model::Scenario) -> Option<PathBuf> {
        // File names stay human-readable: `<name>-<id suffix>.json`. The id
        // suffix keeps two same-named scenarios from overwriting each other
        // on disk while the name keeps the artifact browsable. Legacy files
        // saved as plain `<name>.json` before ids existed still load (see
        // load_scenario's fallbacks).
        let short_id = scenario.id.rsplit('-').next().unwrap_or("0").to_string();
        let file_stem = format!("{}-{}", sanitize(&scenario.name), short_id);
        self.scenario_names.remove(&scenario.name);
        self.saved_scenarios
            .insert(scenario.id.clone(), scenario.clone());
        self.rebuild_scenario_name_index();
        let dir = self.run_dir.as_ref()?.join("scenarios");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}.json", file_stem));
        // Atomic write: temp file then rename (audit item 16).
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&scenario).ok()?).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
        Some(path)
    }

    /// Rebuild the unambiguous-name index over the in-memory scenario set.
    /// A name that maps to exactly one scenario resolves; a name held by two
    /// or more resolves through neither (callers must use the id).
    fn rebuild_scenario_name_index(&mut self) {
        self.scenario_names.clear();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for sc in self.saved_scenarios.values() {
            *counts.entry(sc.name.as_str()).or_insert(0) += 1;
        }
        for sc in self.saved_scenarios.values() {
            if counts.get(sc.name.as_str()).copied().unwrap_or(0) == 1 {
                self.scenario_names.insert(sc.name.clone(), sc.id.clone());
            }
        }
    }

    /// List saved scenarios in this run: the in-memory canonical set (ids)
    /// plus anything persisted in the run dir. Disk entries whose file is
    /// this run's own `<name>-<id-suffix>.json` artifact for an in-memory
    /// scenario are not double-listed.
    pub fn list_saved_scenarios(&self) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = self.saved_scenarios.keys().cloned().collect();
        // Stems this run already accounts for in memory.
        let known_stems: Vec<String> = self
            .saved_scenarios
            .values()
            .map(|sc| {
                let short_id = sc.id.rsplit('-').next().unwrap_or("0");
                format!("{}-{}", sanitize(&sc.name), short_id)
            })
            .collect();
        if let Some(root) = self.run_dir.as_ref() {
            let dir = root.join("scenarios");
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if !out.iter().any(|n| n == stem)
                                && !known_stems.iter().any(|k| k == stem)
                            {
                                out.push(stem.to_string());
                            }
                        }
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Load a scenario by id (preferred) or unambiguous name: memory first,
    /// then the run dir. A name held by two or more in-memory scenarios is
    /// ambiguous and deliberately refuses (use the scenario id). Legacy files
    /// saved as plain `<name>.json` before ids existed are still found.
    pub fn load_scenario(&self, key: &str) -> anyhow::Result<crate::scenario::model::Scenario> {
        // 1. Direct id hit.
        if let Some(s) = self.saved_scenarios.get(key) {
            return Ok(s.clone());
        }
        // 2. Unambiguous-name fallback over the in-memory set.
        if let Some(id) = self.scenario_names.get(key) {
            if let Some(s) = self.saved_scenarios.get(id) {
                return Ok(s.clone());
            }
        }
        let name_held = self.saved_scenarios.values().any(|sc| sc.name == key);
        if name_held {
            // Multiple scenarios carry this name and the name index already
            // refused it: never guess between them.
            return Err(anyhow::anyhow!(
                "scenario name '{}' is ambiguous in run '{}' — pass scenario_id instead",
                key,
                self.id
            ));
        }
        // 3. Not held in memory: run-dir fallbacks for scenarios persisted by
        // an earlier process — a file whose stem ends in this id suffix (when
        // key looks like an id), the legacy plain `<name>.json`, or any file
        // whose embedded name matches.
        let dir = self.scenario_dir()?;
        let short_id = key.rsplit('-').next().unwrap_or("");
        let mut candidates: Vec<PathBuf> = Vec::new();
        if short_id.len() > key.len().saturating_sub(1) {
            // key looks like a bare id: match any `<name>-<id-suffix>.json`.
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.ends_with(&format!("-{}.json", short_id)))
                        .unwrap_or(false)
                    {
                        candidates.push(path);
                    }
                }
            }
        }
        candidates.push(dir.join(format!("{}.json", sanitize(key))));
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                candidates.push(entry.path());
            }
        }
        let mut seen = std::collections::HashSet::new();
        for path in candidates {
            if !seen.insert(path.clone()) {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(sc) = serde_json::from_slice::<crate::scenario::model::Scenario>(&bytes) {
                    if sc.name == key || sc.id == key {
                        return Ok(sc);
                    }
                }
            }
        }
        Err(anyhow::anyhow!(
            "scenario '{}' not found in run '{}' (use its scenario_id, or an unambiguous name)",
            key,
            self.id
        ))
    }

    fn scenario_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self
            .run_dir
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("run '{}' is ephemeral; no scenario storage", self.id))?
            .join("scenarios");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Append findings from an audit pass (typed; audit item 60's foundation).
    pub fn extend_findings(&mut self, findings: Vec<crate::audit::Finding>) {
        self.findings.extend(findings);
    }

    /// Findings accumulated in this run.
    pub fn findings(&self) -> &[crate::audit::Finding] {
        &self.findings
    }

    /// Persist the run manifest (identity + layout).
    fn write_manifest(&self) -> anyhow::Result<()> {
        let Some(dir) = self.run_dir.as_ref() else {
            return Ok(());
        };
        let manifest = RunManifest {
            schema: "tui-lab.run.v1".into(),
            run_id: self.id.clone(),
            started_at: self.started_at,
            launch_spec: self.primary_launch_spec().cloned(),
            sessions: self.session_specs.clone(),
            primary_session: self.primary_session.clone(),
            history_complete: self.history_complete(),
            first_available_seq: self.first_available_seq,
            dropped_records: self.dropped_records,
            closed: self.closed,
        };
        let tmp = dir.join("run.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, dir.join("run.json"))?;
        Ok(())
    }

    /// Retain a completed PTY recording in run memory. When the run is (or
    /// becomes) persistent, `flush` writes it under `recordings/`.
    pub fn hold_recording(&mut self, file_name: String, ndjson: String) {
        self.held_recordings.push((file_name, ndjson));
    }

    /// Wave F item 57: retain a screen capture (SVG/PNG bytes) in run
    /// memory while ephemeral; `flush` writes it under `captures/`.
    pub fn hold_capture(&mut self, file_name: String, body: Vec<u8>, format: &str) {
        self.held_captures
            .push((file_name, body, format.to_string()));
    }

    /// Wave F item 64: fold one native coverage event into the ledger.
    /// Target strings are app-declared (`src/main.rs:42`, `#save.activate`);
    /// the ledger only counts and correlates, never interprets.
    pub fn record_coverage_event(&mut self, session: &str, target: &str) {
        if target.is_empty() {
            return;
        }
        let now = now_ms();
        let entry = self
            .coverage_ledger
            .entry(target.to_string())
            .or_insert_with(|| CoverageEntry {
                hits: 0,
                sessions: Vec::new(),
                first_seen: now,
                last_seen: now,
            });
        entry.hits += 1;
        entry.last_seen = now;
        if !entry.sessions.iter().any(|s| s == session) {
            entry.sessions.push(session.to_string());
        }
    }

    /// Wave F item 64: collect coverage events from a session's native
    /// channel into the ledger. Returns how many events were folded in.
    pub fn collect_native_coverage(
        &mut self,
        session: &str,
        channel: &crate::semantic::native::NativeChannel,
    ) -> usize {
        let events: Vec<(u64, String, String)> = channel
            .events
            .iter()
            .filter(|(_, event, _)| event == "coverage")
            .cloned()
            .collect();
        let n = events.len();
        for (_, _, target) in events {
            self.record_coverage_event(session, &target);
        }
        n
    }

    /// Recordings held in memory (name + event count preview).
    pub fn held_recordings(&self) -> Vec<serde_json::Value> {
        self.held_recordings
            .iter()
            .map(|(name, body)| {
                json!({
                    "file": name,
                    "events": body.lines().count().saturating_sub(1),
                })
            })
            .collect()
    }

    /// Record one focus transition observed during any session's traffic
    /// (the run's focus graph, one entry per semantic focus change).
    pub fn record_focus_transition(
        &mut self,
        session_id: &str,
        from: Option<String>,
        to: Option<String>,
    ) {
        if from == to {
            return;
        }
        self.focus_transitions
            .push((now_ms(), session_id.to_string(), from, to));
    }

    /// The focus-transition ledger.
    pub fn focus_transitions(&self) -> &[(u64, String, Option<String>, Option<String>)] {
        &self.focus_transitions
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
        // Legacy label ledger (unchanged shape, skips no-op transitions).
        if from_label != to_label {
            self.focus_transitions.push((
                now_ms(),
                session_id.to_string(),
                from_label.clone(),
                to_label.clone(),
            ));
        }
        // ID-keyed graph.
        if let (Some(f), Some(t)) = (from_id, to_id) {
            self.focus_graph.transition(f, t, to_label.as_deref(), via);
        }
    }

    /// Whether the run is marked closed (`tui_run close`).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

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
    ) {
        let seq = self.transaction_count;
        self.transaction_count += 1;
        let mut record = TransactionRecord::from_interaction(seq, session, tx);
        record.seq = seq;
        self.push_ledger(record);
    }

    /// Record a non-frame interaction (wait/observe) at evidence level.
    ///
    /// Settlement is honestly `skipped`: no settle wait ran for this entry,
    /// and the old hardcoded `settled: true` (with empty hashes) was a lie.
    /// Same bounding + declared eviction as [`Self::record_interaction`]
    /// (P0 fix 5) — one bounded path, not one bounded and one unbounded.
    pub fn record_event(&mut self, session: &str, action: &str) {
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
            persisted_action: None,
        });
    }

    /// Shared bounded push with declared eviction (P0 fix 5).
    fn push_ledger(&mut self, record: TransactionRecord) {
        // Persistent runs append immediately (Wave B item 14): the file is
        // the authoritative history, memory is only a bounded window. The
        // watermark rolls back on append failure so flush retries the tail.
        if let Some(dir) = self.run_dir.clone() {
            let flushed = self.ledger_flushed_upto;
            self.transactions.push(record);
            if self.append_ledger_incremental(&dir).is_err() {
                self.ledger_flushed_upto = flushed;
            }
        } else {
            self.transactions.push(record);
        }
        // Declared eviction (P0 fix 5) — for persistent runs this trims the
        // memory window only (disk already holds the records); for
        // ephemeral runs it is a real loss, which the manifest declares.
        if self.transactions.len() >= MAX_TRANSACTION_RECORDS + 512 {
            let drop = MAX_TRANSACTION_RECORDS / 2;
            let new_first = self.transactions[drop].seq;
            self.transactions.drain(..drop);
            self.dropped_records += drop as u64;
            self.first_available_seq = Some(new_first);
        }
    }

    /// The transaction ledger (bounded; see [`Self::record_transaction`]).
    pub fn transactions(&self) -> &[TransactionRecord] {
        &self.transactions
    }

    /// True transaction count, including ledger entries already evicted.
    pub fn transaction_total(&self) -> u64 {
        self.transaction_count
    }

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

    /// Status snapshot for `tui_run status` (goal spec shape).
    ///
    /// `sessions` comes from the caller: the run records the primary launch
    /// spec; the live session list lives in the SessionManager, which the run
    /// does not own. The MCP layer fills it in.
    pub fn status(&self, sessions: Vec<serde_json::Value>) -> serde_json::Value {
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
        })
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
                std::fs::write(&tmp, serde_json::to_vec_pretty(sc)?)?;
                std::fs::rename(&tmp, &path)?;
            }
        }
        // State graph.
        let graph = self.state_graph.export();
        let tmp = dir.join("state_graph.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&graph)?)?;
        std::fs::rename(&tmp, dir.join("state_graph.json"))?;
        // Focus graph (legacy label ledger + the ID-keyed FocusGraph).
        let ftmp = dir.join("focus_graph.json.tmp");
        std::fs::write(&ftmp, serde_json::to_vec_pretty(&self.focus_transitions)?)?;
        std::fs::rename(&ftmp, dir.join("focus_graph.json"))?;
        let gtmp = dir.join("focus_graph_ids.json.tmp");
        std::fs::write(&gtmp, serde_json::to_vec_pretty(&self.focus_graph)?)?;
        std::fs::rename(&gtmp, dir.join("focus_graph_ids.json"))?;
        // Transaction ledger (Wave-2 item 15 + Wave B item 14): NDJSON,
        // APPENDED incrementally — every record whose seq exceeds
        // `ledger_flushed_upto` is appended now, so a crash loses at most
        // the last in-flight record instead of the whole unflushed tail.
        // The eviction window still bounds memory; the file is the
        // authoritative history for persistent runs.
        self.append_ledger_incremental(&dir)?;
        // Terminal-event logs (Wave B item 12/14).
        let ev_dir = dir.join("events");
        std::fs::create_dir_all(&ev_dir)?;
        for (session, events) in std::mem::take(&mut self.held_events) {
            let safe = sanitize(&session);
            let mut body = String::new();
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
            );
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
                    );
                }
                Err(_) => still_held_caps.push((name, body, format)),
            }
        }
        self.held_captures = still_held_caps;
        // Findings.
        let fdir = dir.join("findings");
        std::fs::create_dir_all(&fdir)?;
        let ftmp = dir.join("findings.json.tmp");
        std::fs::write(&ftmp, serde_json::to_vec_pretty(&self.findings)?)?;
        std::fs::rename(&ftmp, dir.join("findings.json"))?;
        // Wave F item 64: the native coverage ledger.
        if !self.coverage_ledger.is_empty() {
            let ctmp = dir.join("coverage.json.tmp");
            std::fs::write(&ctmp, serde_json::to_vec_pretty(&self.coverage_ledger)?)?;
            std::fs::rename(&ctmp, dir.join("coverage.json"))?;
        }
        Ok(())
    }

    /// Resolve the per-run artifact directory from a caller-supplied base.
    ///
    /// A base whose last component is `runs` is treated as the runs directory
    /// itself (the goal spec's persist example) and holds the run directly:
    /// `<base>/<run-id>`. Any other base is a repo root:
    /// `<base>/.tui-lab/runs/<run-id>`. Session-cwd resolution always lands
    /// on the repo-root form.
    fn runs_dir_for(base: &std::path::Path, id: &str) -> PathBuf {
        let is_runs_dir = base
            .file_name()
            .map(|f| f == std::ffi::OsStr::new("runs"))
            .unwrap_or(false);
        if is_runs_dir {
            base.join(id)
        } else {
            base.join(".tui-lab").join("runs").join(id)
        }
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
        // Flush everything accumulated while ephemeral into the new root.
        self.flush()?;
        self.write_manifest()?;
        Ok(root)
    }

    /// Append every ledger record not yet on disk to
    /// `<run>/transactions.jsonl` (Wave B item 14). Called by `flush` (and
    /// by `record_interaction` for persistent runs) so the file grows
    /// incrementally with the run. Evicted records are never rewritten —
    /// they are already in the file if the run was persistent when they
    /// landed, and the manifest's declared gap covers them if not.
    fn append_ledger_incremental(&mut self, dir: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;
        let path = dir.join("transactions.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for tx in &self.transactions {
            if tx.seq >= self.ledger_flushed_upto {
                let line = serde_json::to_string(tx)?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                self.ledger_flushed_upto = tx.seq + 1;
            }
        }
        Ok(())
    }

    /// Assign the next citable frame id (Wave B item 11) and stamp the
    /// frame with run provenance. Returns the id (`frame:N`).
    pub fn register_frame(&mut self, frame: &mut crate::backend::CanonicalFrame) -> u64 {
        self.next_frame_id += 1;
        let id = self.next_frame_id;
        frame.assign_frame_id(id);
        frame.run_id = Some(self.id.clone());
        id
    }

    /// Hold one session's drained terminal events for persistence (Wave B
    /// item 14). Sessions own the live queue; the run keeps the export.
    pub fn hold_events(&mut self, session: &str, events: Vec<crate::events::TerminalEvent>) {
        if events.is_empty() {
            return;
        }
        self.held_events.push((session.to_string(), events));
    }

    /// Register a produced artifact and get its typed ref (Wave B item 15).
    pub fn register_artifact(
        &mut self,
        kind: ArtifactKind,
        path: Option<PathBuf>,
        size: Option<u64>,
        session: Option<String>,
        summary: impl Into<String>,
    ) -> ArtifactRef {
        let n = self.artifacts.len() + 1;
        let r = ArtifactRef {
            id: format!("art-{}", n),
            kind,
            path,
            size,
            session,
            summary: summary.into(),
        };
        self.artifacts.push(r.clone());
        r
    }

    /// All artifacts registered in this run.
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    /// Counts for `tui_run status`.
    pub fn counts(&self) -> serde_json::Value {
        json!({
            "transactions": self.transaction_count,
            "transactions_in_ledger": self.transactions.len(),
            "history_complete": self.history_complete(),
            "dropped_records": self.dropped_records,
            "first_available_seq": self.first_available_seq,
            "events": self.event_count,
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

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".into()
    } else {
        cleaned
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_context_ephemeral_has_no_dir() {
        let run = RunContext::ephemeral();
        assert!(run.run_dir().is_none());
        assert!(run.active_recordings().is_empty());
    }

    #[test]
    fn scenario_recording_lifecycle() {
        let mut run = RunContext::ephemeral();
        let id1 = run.begin_scenario_recording("demo", "sess-A", 1);
        // Names are not identities: a second recording with the same name is
        // legitimate (different session or generation).
        let id2 = run.begin_scenario_recording("demo", "sess-B", 1);
        assert_ne!(id1, id2);
        // Traffic for sess-A gen 1 lands ONLY in the sess-A recording.
        run.record_scenario_act(
            "sess-A",
            1,
            serde_json::json!({"action": "key", "key": "enter"}),
        );
        run.record_scenario_assert(
            "sess-A",
            1,
            serde_json::json!({"assertion": "text", "text": "OK"}),
        );
        // sess-B's traffic never lands in sess-A's recording.
        run.record_scenario_act(
            "sess-B",
            1,
            serde_json::json!({"action": "key", "key": "escape"}),
        );
        let a = run.finish_scenario_recording(id1.as_str()).expect("A");
        assert_eq!(a.step_count(), 2, "sess-A recording: {:?}", a.steps);
        let b = run.finish_scenario_recording(id2.as_str()).expect("B");
        assert_eq!(b.step_count(), 1, "sess-B recording: {:?}", b.steps);
        // A stale generation (after restart) must not absorb traffic either.
        let id3 = run.begin_scenario_recording("stale", "sess-A", 2);
        run.record_scenario_act(
            "sess-A",
            7,
            serde_json::json!({"action": "key", "key": "enter"}),
        );
        let c = run.finish_scenario_recording(id3.as_str()).expect("C");
        assert_eq!(c.step_count(), 0, "wrong generation must not match");
        assert!(run.active_recordings().is_empty());
    }

    /// Wave-2 item 15: the run keeps a reconstructable transaction ledger,
    /// not just a counter, and flushes it to transactions.jsonl.
    #[test]
    fn transaction_ledger_records_and_flushes() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // Build a real interaction transaction through the canonical executor.
        let mut s = crate::session::state::Session::new("ledger-sess".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('x'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        let tx = crate::execution::execute_act(
            &mut s,
            &crate::execution::CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('q')),
            },
            60,
            1000,
            false,
        )
        .expect("execute");

        run.record_interaction("ledger-sess", &tx);
        run.record_event("ledger-sess", "wait");

        assert_eq!(run.transaction_total(), 2);
        let ledger = run.transactions();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].seq, 0);
        assert_eq!(ledger[0].action, "key");
        assert_eq!(ledger[1].action, "wait");
        assert_eq!(ledger[0].before_structure, tx.before().structure_hash);
        assert_eq!(ledger[0].after_structure, tx.after().structure_hash);

        run.flush().expect("flush");
        let body =
            std::fs::read_to_string(run.run_dir().expect("run dir").join("transactions.jsonl"))
                .expect("ledger file");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one NDJSON record per line: {body}");
        let first: TransactionRecord = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(first.action, "key");
    }

    /// Wave B item 14: a PERSISTENT run appends each ledger record to
    /// transactions.jsonl immediately — the file grows with the run, so a
    /// crash cannot lose the unflushed tail.
    #[test]
    fn persistent_ledger_appends_incrementally() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // Record BEFORE any flush: the file must already hold the record.
        run.record_event("s1", "wait");
        let path = run.run_dir().expect("dir").join("transactions.jsonl");
        let body = std::fs::read_to_string(&path).expect("ledger exists pre-flush");
        assert_eq!(body.lines().count(), 1, "appended immediately: {body}");

        run.record_event("s1", "wait");
        let body = std::fs::read_to_string(&path).expect("reread");
        assert_eq!(body.lines().count(), 2, "second append: {body}");

        // And flush is idempotent on the file (no duplicate lines).
        run.flush().expect("flush");
        let body = std::fs::read_to_string(&path).expect("reread");
        assert_eq!(body.lines().count(), 2, "flush must not duplicate");
    }

    /// Wave B item 11: frames get citable per-run ids and provenance.
    #[test]
    fn frames_get_citable_ids() {
        let mut run = RunContext::ephemeral();
        let mut f =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 3, 9);
        let id = run.register_frame(&mut f);
        assert_eq!(id, 1);
        assert_eq!(f.frame_id, Some(1));
        assert_eq!(f.cite(), "frame:1");
        assert_eq!(f.run_id.as_deref(), Some(run.id.as_str()));

        let mut g =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 4, 10);
        let id2 = run.register_frame(&mut g);
        assert_eq!(id2, 2, "monotonic per run");
        assert_eq!(run.counts()["frames"], 2);
    }

    /// Wave B item 15: artifacts register as typed refs, both persisted and
    /// held (ephemeral), and travel into run status.
    #[test]
    fn artifact_refs_register_and_cite() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        let rel = std::path::PathBuf::from("recordings").join("s1-1.cast");
        let a = run.register_artifact(
            ArtifactKind::Recording,
            Some(rel.clone()),
            Some(123),
            Some("s1".into()),
            "test recording",
        );
        assert_eq!(a.id, "art-1");
        assert_eq!(a.cite(), "art-1:recording");
        assert_eq!(run.artifacts().len(), 1);

        // Ephemeral registration (no path) still gets a ref.
        let b = run.register_artifact(
            ArtifactKind::EventLog,
            None,
            Some(456),
            Some("s2".into()),
            "held events",
        );
        assert_eq!(b.id, "art-2");
        assert!(b.path.is_none());

        let status = run.status(vec![]);
        let arts = status["artifacts"].as_array().expect("artifacts in status");
        assert_eq!(arts.len(), 2);
    }

    /// Wave B item 12/14: session event queues drain into per-session
    /// events/<session>.jsonl on flush, registered as typed artifacts.
    #[test]
    fn event_logs_flush_to_artifacts() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        let mut s = crate::session::state::Session::new("ev-sess".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('ev'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        // Two observations: ProcessStarted (+ possibly ScreenChanged).
        let _ = s.observe(50).expect("observe 1");
        let _ = s.observe(50).expect("observe 2");
        let drained = s.drain_events();
        assert!(!drained.is_empty(), "observe must emit events");
        assert!(
            drained.iter().any(|e| e.kind.name() == "process_started"),
            "first observation emits ProcessStarted"
        );

        run.hold_events("ev-sess", drained);
        run.flush().expect("flush");
        let log = std::fs::read_to_string(
            run.run_dir()
                .expect("dir")
                .join("events")
                .join("ev-sess.jsonl"),
        )
        .expect("event log");
        assert!(log.contains("\"type\":\"process_started\""), "{log}");
        assert!(
            run.artifacts()
                .iter()
                .any(|a| a.kind == ArtifactKind::EventLog),
            "event log registered as artifact"
        );
    }

    #[test]
    fn persistent_run_writes_manifest_and_scenario_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        assert!(run.run_dir().is_some());
        assert!(run.run_dir().unwrap().join("run.json").exists());

        run.set_launch_spec(
            "primary-sess",
            crate::session::state::LaunchSpec::new("python3", 80, 24),
        );

        let scenario = crate::scenario::model::Scenario::new("roundtrip")
            .act(serde_json::json!({"action": "key", "key": "enter"}))
            .wait(serde_json::json!({"condition": "text", "text": "SAVED"}));
        let scenario_id = scenario.id.clone();
        let path = run.save_scenario(scenario).expect("save");
        assert!(path.exists());

        let ids = run.list_saved_scenarios().expect("list");
        assert_eq!(ids, vec![scenario_id.clone()]);

        // Load by id…
        let loaded = run.load_scenario(&scenario_id).expect("load by id");
        assert_eq!(loaded.step_count(), 2);
        assert_eq!(loaded.name, "roundtrip");
        // …and by unambiguous name.
        let loaded_by_name = run.load_scenario("roundtrip").expect("load by name");
        assert_eq!(loaded_by_name.id, scenario_id);
    }

    #[test]
    fn same_named_scenarios_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        let a = crate::scenario::model::Scenario::new("login")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let b = crate::scenario::model::Scenario::new("login")
            .act(serde_json::json!({"action": "key", "key": "tab"}));
        let id_a = a.id.clone();
        let id_b = b.id.clone();
        assert_ne!(id_a, id_b);
        run.save_scenario(a).expect("save a");
        run.save_scenario(b).expect("save b");

        assert_eq!(run.saved_scenarios.len(), 2, "both scenarios kept");
        // Ambiguous name resolves through neither…
        assert!(run.load_scenario("login").is_err());
        // …but each id resolves to its own steps.
        let loaded_a = run.load_scenario(&id_a).expect("load a");
        let loaded_b = run.load_scenario(&id_b).expect("load b");
        assert_ne!(
            loaded_a.steps, loaded_b.steps,
            "id collision would overwrite one scenario with the other"
        );
    }

    #[test]
    fn promote_preserves_identity_and_flushes_state() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        let run_id = run.id.clone();

        // Accumulate state while ephemeral: checkpoint, scenario, findings,
        // counters, state graph.
        run.bump_transaction();
        run.bump_event();
        let cp_name = run.checkpoints.save(
            "sess-P",
            1,
            Some("promoted-cp".into()),
            &crate::screen::ScreenState::new(80, 24),
            None,
        );
        let promoted_scenario = crate::scenario::model::Scenario::new("promoted-flow")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let expected_stem = format!(
            "promoted-flow-{}",
            promoted_scenario.id.rsplit('-').next().unwrap_or("0")
        );
        run.save_scenario(promoted_scenario);
        let graph_root = base.path().join(".tui-lab").join("runs").join(&run_id);

        let root = run.promote(base.path()).expect("promote");
        assert_eq!(root, graph_root, "promote uses the caller-provided base");
        assert_eq!(run.id, run_id, "promotion preserves run identity");
        assert!(graph_root.join("run.json").exists(), "manifest flushed");
        assert!(
            graph_root
                .join("scenarios")
                .join(format!("{}.json", expected_stem))
                .exists(),
            "ephemeral-era scenario flushed into durable root"
        );

        // In-memory checkpoint survived promotion (reroot, not replace).
        let screen = crate::screen::ScreenState::new(80, 24);
        let sem = crate::semantic::analyze(&screen);
        let cmp = run
            .checkpoints
            .compare("sess-P", &cp_name, &screen, Some(&sem));
        assert!(
            cmp.is_ok(),
            "checkpoint must survive promotion: {:?}",
            cmp.err()
        );
        assert!(
            graph_root
                .join("checkpoints")
                .join("sess-P")
                .join("promoted-cp.json")
                .exists(),
            "carried checkpoint re-persisted into the durable root"
        );

        // Closing marks + persists the flag; second close is a no-op.
        run.close().expect("close");
        assert!(run.is_closed());
        let text = std::fs::read_to_string(graph_root.join("run.json")).expect("manifest");
        assert!(text.contains("\"closed\": true"));
        run.close().expect("second close is a no-op");

        // Ephemeral close: no dir, no error, still marked closed.
        let mut eph = RunContext::ephemeral();
        eph.close().expect("ephemeral close");
        assert!(eph.is_closed() && eph.run_dir().is_none());
    }

    #[test]
    fn ephemeral_recording_survives_promotion() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        // Recording stopped while ephemeral: held in memory, not on disk.
        run.hold_recording("sess-A-123.cast".to_string(), "x\ny\n".to_string());
        assert_eq!(run.held_recordings().len(), 1);
        let root = run.promote(base.path()).expect("promote");
        let written = root.join("recordings").join("sess-A-123.cast");
        assert!(written.exists(), "held recording flushed at promotion");
        let body = std::fs::read_to_string(written).expect("body");
        assert_eq!(body, "x\ny\n");
        assert!(run.held_recordings().is_empty(), "flush drains the hold");
    }

    #[test]
    fn focus_transitions_record_and_flush() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        run.record_focus_transition("s", None, Some("file".into()));
        run.record_focus_transition("s", Some("file".into()), Some("file".into()));
        run.record_focus_transition("s", Some("file".into()), Some("menu".into()));
        // No-op transitions (unchanged focus) are not recorded.
        assert_eq!(run.focus_transitions().len(), 2);
        let root = run.promote(base.path()).expect("promote");
        let body = std::fs::read_to_string(root.join("focus_graph.json")).expect("focus graph");
        assert!(body.contains("\"menu\""), "ledger persisted: {body}");
        assert!(run.counts()["focus_transitions"].as_u64() >= Some(2));
    }

    #[test]
    fn status_reports_ephemeral_and_persistent_modes() {
        let eph = RunContext::ephemeral();
        let st = eph.status(Vec::new());
        assert_eq!(st["mode"], "ephemeral");
        assert_eq!(st["persistent"], false);
        assert!(st["artifact_root"].is_null());

        let base = tempfile::tempdir().expect("base");
        let per = RunContext::persistent(base.path()).expect("persistent");
        let st2 = per.status(vec![serde_json::Value::String("sess-x".into())]);
        assert_eq!(st2["mode"], "persistent");
        assert_eq!(st2["persistent"], true);
        assert_eq!(st2["sessions"], serde_json::json!(["sess-x"]));
        assert!(st2["artifact_root"].as_str().is_some());
    }

    // ── Wave G item 74: run restore ─────────────────────────────────────

    /// Build a durable run with everything restore is expected to bring
    /// back: launch spec, transactions, findings, focus ledger, scenarios,
    /// a held recording.
    fn persisted_fixture(base: &std::path::Path) -> (String, std::path::PathBuf) {
        let mut run = RunContext::ephemeral();
        run.set_launch_spec(
            "sess-fix",
            crate::session::state::LaunchSpec::new("python3", 90, 26),
        );
        run.record_event("sess-fix", "wait");
        run.extend_findings(vec![crate::audit::Finding {
            id: "RESTORE-ME".into(),
            severity: "warn".into(),
            category: "test".into(),
            summary: "finding that must survive restore".into(),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                "fixture",
                "restore test",
            )],
            confidence: 1.0,
            reproduction: None,
        }]);
        run.record_focus_transition("sess-fix", None, Some("file".into()));
        let sc = crate::scenario::model::Scenario::new("restored-flow")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let expected_stem = format!("restored-flow-{}", sc.id.rsplit('-').next().unwrap_or("0"));
        run.save_scenario(sc);
        run.hold_recording("sess-fix-42.cast".into(), "x\n".into());

        let root = run.promote(base).expect("promote");
        assert!(root.join("run.json").exists());
        (expected_stem, root)
    }

    #[test]
    fn restore_brings_back_identity_ledger_findings_scenarios() {
        let base = tempfile::tempdir().expect("base");
        let (scenario_stem, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();

        let restored = RunContext::restore(&root).expect("restore");
        assert_eq!(restored.id, run_id, "identity survives");
        assert!(restored.run_dir().is_some(), "durable root survives");
        assert_eq!(restored.transaction_total(), 1, "ledger count restored");
        assert_eq!(restored.transactions().len(), 1);
        assert_eq!(
            restored.transactions()[0].settle,
            "skipped",
            "non-interaction entries keep their honest settle"
        );
        assert_eq!(restored.findings().len(), 1);
        assert_eq!(restored.findings()[0].id, "RESTORE-ME");
        assert_eq!(restored.focus_transitions().len(), 1);
        // Scenario listable + loadable by id (name index rebuilt). The list
        // is keyed by id; the unambiguous name resolves to the same thing.
        let listed = restored.list_saved_scenarios().expect("list");
        assert_eq!(listed.len(), 1, "scenario listed after restore: {listed:?}");
        let loaded = restored
            .load_scenario("restored-flow")
            .expect("load by unambiguous name");
        assert_eq!(loaded.step_count(), 1);
        assert!(root
            .join("scenarios")
            .join(format!("{scenario_stem}.json"))
            .exists());
        // Launch specs restored → primary cwd resolvable again.
        let specs = restored.launch_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].1.cols, 90);
        assert_eq!(restored.primary_session_cwd(), None, "fixture cwd was None");
        // Held recording flushed at promotion → restored as an artifact ref.
        assert!(
            restored
                .artifacts()
                .iter()
                .any(|a| a.summary.contains("pty recording")),
            "recording re-registered: {:?}",
            restored.artifacts()
        );
    }

    #[test]
    fn restored_run_continues_appending_to_the_same_dir() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();

        let mut restored = RunContext::restore(&root).expect("restore");
        restored.record_event("sess-fix", "wait");
        // The new record appends to the SAME ledger file (id preserved, no
        // fork into a new run dir).
        let body = std::fs::read_to_string(root.join("transactions.jsonl")).expect("ledger");
        assert_eq!(body.lines().count(), 2, "ledger appended in place: {body}");
        // And the manifest still declares the same run id.
        let m = crate::run::manifest::load(&root).expect("manifest");
        assert_eq!(m.run_id, run_id);
    }

    #[test]
    fn restore_reports_torn_ledger_tail_honestly() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // Simulate a crash mid-append: a torn final line.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(root.join("transactions.jsonl"))
                .expect("open ledger");
            write!(f, "{{\"seq\":9,\"at\":1,\"session\":\"sess-fi").unwrap();
        }
        let restored = RunContext::restore(&root).expect("restore still succeeds");
        assert_eq!(restored.transactions().len(), 1, "only the good line");
        assert_eq!(
            restored.dropped_records, 1,
            "the torn line is DECLARED, not dropped silently"
        );
        assert!(
            !restored.history_complete(),
            "torn tail breaks completeness"
        );
    }

    #[test]
    fn resolve_run_dir_accepts_both_root_shapes() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();
        // Repo-root shape.
        assert_eq!(
            RunContext::resolve_run_dir(base.path(), &run_id),
            Some(root.clone())
        );
        // Runs-dir shape (the spec example).
        let runs = base.path().join(".tui-lab").join("runs");
        assert_eq!(
            RunContext::resolve_run_dir(&runs, &run_id),
            Some(root.clone())
        );
        // Unknown id → None.
        assert_eq!(RunContext::resolve_run_dir(base.path(), "run-nope"), None);
    }

    #[test]
    fn list_persisted_names_runs_and_corrupt_entries() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // A corrupt entry (no manifest).
        let scratch = base.path().join(".tui-lab").join("runs").join("scratch");
        std::fs::create_dir_all(&scratch).expect("scratch");
        let listed = RunContext::list_persisted(base.path()).expect("list");
        let run_entry = listed
            .iter()
            .find(|e| {
                e.get("run_id").and_then(|r| r.as_str())
                    == Some(root.file_name().unwrap().to_string_lossy().as_ref())
            })
            .expect("fixture run listed");
        assert_eq!(run_entry["closed"], false);
        assert_eq!(run_entry["history_complete"], true);
        assert_eq!(run_entry["ledger_transactions"], 1);
        let skipped = listed
            .iter()
            .find(|e| e.get("skipped").is_some())
            .expect("corrupt dir named, not hidden");
        assert!(
            skipped["path"].as_str().unwrap_or("").ends_with("scratch"),
            "{skipped}"
        );
    }

    /// A closed run restores as closed — resume never resurrects a
    /// finished run into pretending it is open.
    #[test]
    fn closed_run_stays_closed_after_restore() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        {
            let mut run = RunContext::restore(&root).expect("restore");
            run.close().expect("close");
        }
        let restored = RunContext::restore(&root).expect("restore again");
        assert!(restored.is_closed(), "closed flag is durable");
        assert_eq!(restored.status(Vec::new())["closed"], true);
    }
}
