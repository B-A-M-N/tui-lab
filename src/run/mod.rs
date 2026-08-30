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

pub mod manifest;
pub mod recording_scope;

pub use manifest::RunManifest;

use crate::checkpoint::store::CheckpointStore;
use crate::exploration::state_graph::{ExplorationBudget, StateGraph};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bound on the in-memory transaction ledger (re-review Wave-2 item
/// 15). Frames are heavy; the ledger holds evidence-level records. When the
/// bound is hit the oldest half is evicted; the total counter is never
/// reset, so `transactions: 47` stays honest while the ledger holds the
/// most recent half-bound.
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
    /// Whether the post-action settle condition was met.
    pub settled: bool,
    /// Why the settle resolved (or timed out), if reported.
    pub settle_reason: Option<String>,
    /// Structure hash before the action.
    pub before_structure: String,
    /// Structure hash after the action (the matching/settled frame).
    pub after_structure: String,
    /// Cells changed across the transition.
    pub changed_cells: usize,
    /// Settle latency in milliseconds.
    pub elapsed_ms: u64,
    /// The typed action (Wave-2 item 10) — present when the transaction came
    /// through the canonical executor, so the ledger can *replay*, not just
    /// count. Skipped for non-interaction ledger entries ("wait").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<crate::execution::CanonicalAction>,
}

impl TransactionRecord {
    /// Build a record from a live [`crate::execution::InteractionTransaction`].
    pub fn from_interaction(
        seq: u64,
        session: &str,
        tx: &crate::execution::InteractionTransaction,
    ) -> Self {
        TransactionRecord {
            seq,
            at: now_ms(),
            session: session.to_string(),
            action: tx.action.clone(),
            settled: tx.settled,
            settle_reason: tx.settle_reason.clone(),
            before_structure: tx.before.structure_hash.clone(),
            after_structure: tx.after.structure_hash.clone(),
            changed_cells: tx.transition.screen_diff.changed_cells,
            elapsed_ms: tx.elapsed_ms,
            canonical: tx.canonical.clone(),
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
    /// The launch spec of the primary session this run was opened for, if any.
    launch_spec: Option<crate::session::state::LaunchSpec>,
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
    /// Focus-transition ledger: (unix_ms, session, from, to) recorded from
    /// semantic analysis of every observation. The run's focus graph.
    focus_transitions: Vec<(u64, String, Option<String>, Option<String>)>,
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
    /// Set by `tui_run close`. Sessions are NOT touched by closing.
    closed: bool,
}

impl RunContext {
    /// Create an in-memory run (no artifacts written).
    pub fn ephemeral() -> Self {
        RunContext {
            id: format!("run-{}", uuid::Uuid::new_v4().simple()),
            started_at: now_ms(),
            run_dir: None,
            launch_spec: None,
            checkpoints: CheckpointStore::new(),
            recorders: HashMap::new(),
            saved_scenarios: HashMap::new(),
            scenario_names: HashMap::new(),
            held_recordings: Vec::new(),
            focus_transitions: Vec::new(),
            state_graph: StateGraph::new(ExplorationBudget::default()),
            findings: Vec::new(),
            transactions: Vec::new(),
            transaction_count: 0,
            event_count: 0,
            closed: false,
        }
    }

    /// Create a persistent run rooted at `.tui-lab/runs/<run-id>` under
    /// `base` (defaults to the process cwd when `base` is `None`).
    pub fn persistent(base: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut run = RunContext::ephemeral();
        let root = match base {
            Some(b) => Self::runs_dir_for(b, &run.id),
            None => PathBuf::from(".tui-lab").join("runs").join(&run.id),
        };
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

    /// Record the launch spec of the session this run is attached to.
    pub fn set_launch_spec(&mut self, spec: crate::session::state::LaunchSpec) {
        self.launch_spec = Some(spec);
        self.write_manifest().ok();
    }

    pub fn launch_spec(&self) -> Option<&crate::session::state::LaunchSpec> {
        self.launch_spec.as_ref()
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
            launch_spec: self.launch_spec.clone(),
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
    /// assigned here from the run's own counter. Bounded; when the ledger
    /// is full, the oldest half is dropped (counters keep the true total).
    pub fn record_interaction(
        &mut self,
        session: &str,
        tx: &crate::execution::InteractionTransaction,
    ) {
        let seq = self.transaction_count;
        self.transaction_count += 1;
        let mut record = TransactionRecord::from_interaction(seq, session, tx);
        record.seq = seq;
        if self.transactions.len() >= MAX_TRANSACTION_RECORDS {
            let drop = MAX_TRANSACTION_RECORDS / 2;
            self.transactions.drain(..drop);
        }
        self.transactions.push(record);
    }

    /// Record a non-frame interaction (wait/observe) at evidence level.
    pub fn record_event(&mut self, session: &str, action: &str) {
        let seq = self.transaction_count;
        self.transaction_count += 1;
        self.transactions.push(TransactionRecord {
            seq,
            at: now_ms(),
            session: session.to_string(),
            action: action.to_string(),
            settled: true,
            settle_reason: None,
            before_structure: String::new(),
            after_structure: String::new(),
            changed_cells: 0,
            elapsed_ms: 0,
            canonical: None,
        });
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
        self.launch_spec.as_ref().and_then(|s| s.cwd.as_deref())
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
        // Focus graph.
        let ftmp = dir.join("focus_graph.json.tmp");
        std::fs::write(&ftmp, serde_json::to_vec_pretty(&self.focus_transitions)?)?;
        std::fs::rename(&ftmp, dir.join("focus_graph.json"))?;
        // Transaction ledger (Wave-2 item 15): NDJSON, one record per line,
        // so partial reads and appends stay possible.
        let ttmp = dir.join("transactions.jsonl.tmp");
        let mut lines = String::new();
        for tx in &self.transactions {
            lines.push_str(&serde_json::to_string(tx)?);
            lines.push('\n');
        }
        std::fs::write(&ttmp, lines)?;
        std::fs::rename(&ttmp, dir.join("transactions.jsonl"))?;
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
        // Findings.
        let fdir = dir.join("findings");
        std::fs::create_dir_all(&fdir)?;
        let ftmp = dir.join("findings.json.tmp");
        std::fs::write(&ftmp, serde_json::to_vec_pretty(&self.findings)?)?;
        std::fs::rename(&ftmp, dir.join("findings.json"))?;
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

    /// Counts for `tui_run status`.
    pub fn counts(&self) -> serde_json::Value {
        json!({
            "transactions": self.transaction_count,
            "transactions_in_ledger": self.transactions.len(),
            "events": self.event_count,
            "checkpoints": self.checkpoints.count(),
            "scenarios": self.saved_scenarios.len(),
            "findings": self.findings.len(),
            "held_recordings": self.held_recordings.len(),
            "focus_transitions": self.focus_transitions.len(),
            "state_graph_states": self.state_graph.state_count(),
            "state_graph_transitions": self.state_graph.transition_count(),
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
        let mut run = RunContext::persistent(Some(tmp.path())).expect("run");

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
        assert_eq!(ledger[0].before_structure, tx.before.structure_hash);
        assert_eq!(ledger[0].after_structure, tx.after.structure_hash);

        run.flush().expect("flush");
        let body =
            std::fs::read_to_string(run.run_dir().expect("run dir").join("transactions.jsonl"))
                .expect("ledger file");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one NDJSON record per line: {body}");
        let first: TransactionRecord = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(first.action, "key");
    }

    #[test]
    fn persistent_run_writes_manifest_and_scenario_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(Some(tmp.path())).expect("run");
        assert!(run.run_dir().is_some());
        assert!(run.run_dir().unwrap().join("run.json").exists());

        run.set_launch_spec(crate::session::state::LaunchSpec::new("python3", 80, 24));

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
        let mut run = RunContext::persistent(Some(tmp.path())).expect("run");

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
        let per = RunContext::persistent(Some(base.path())).expect("persistent");
        let st2 = per.status(vec![serde_json::Value::String("sess-x".into())]);
        assert_eq!(st2["mode"], "persistent");
        assert_eq!(st2["persistent"], true);
        assert_eq!(st2["sessions"], serde_json::json!(["sess-x"]));
        assert!(st2["artifact_root"].as_str().is_some());
    }
}
