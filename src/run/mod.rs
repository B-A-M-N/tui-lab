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

pub use manifest::RunManifest;

use crate::checkpoint::store::CheckpointStore;
use crate::exploration::state_graph::{ExplorationBudget, StateGraph};
use crate::scenario::recorder::ScenarioRecorder;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Scenario recorders by name (recording in progress).
    recorders: HashMap<String, ScenarioRecorder>,
    /// Completed scenarios for this run. Memory is canonical during the live
    /// run (so ephemeral runs still list/export); persistence to the run dir
    /// is an artifact layer on top, not the source of truth.
    saved_scenarios: HashMap<String, crate::scenario::model::Scenario>,
    /// Exploration state graph (owned here so audits/exploration share it).
    pub state_graph: StateGraph,
    /// Findings emitted during this run (audit results).
    findings: Vec<crate::audit::Finding>,
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
            state_graph: StateGraph::new(ExplorationBudget::default()),
            findings: Vec::new(),
        }
    }

    /// Create a persistent run rooted at `.tui-lab/runs/<run-id>` under
    /// `base` (defaults to the process cwd when `base` is `None`).
    pub fn persistent(base: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut run = RunContext::ephemeral();
        let root = match base {
            Some(b) => b.join(".tui-lab").join("runs").join(&run.id),
            None => PathBuf::from(".tui-lab").join("runs").join(&run.id),
        };
        std::fs::create_dir_all(root.join("checkpoints"))?;
        std::fs::create_dir_all(root.join("scenarios"))?;
        std::fs::create_dir_all(root.join("recordings"))?;
        std::fs::create_dir_all(root.join("findings"))?;
        run.checkpoints = CheckpointStore::with_run_dir(
            root.join("checkpoints").to_string_lossy().to_string(),
        );
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

    /// Start recording a scenario under `name`. Returns false if a recorder
    /// with that name already exists.
    pub fn begin_scenario_recording(&mut self, name: &str) -> bool {
        if self.recorders.contains_key(name) {
            return false;
        }
        self.recorders
            .insert(name.to_string(), ScenarioRecorder::new(name));
        true
    }

    /// Record an act step into the named scenario recorder (audit item 22).
    pub fn record_scenario_act(&mut self, name: &str, params: serde_json::Value) {
        if let Some(r) = self.recorders.get_mut(name) {
            r.record_act(params);
        }
    }

    /// Record a wait step into the named scenario recorder.
    pub fn record_scenario_wait(&mut self, name: &str, params: serde_json::Value) {
        if let Some(r) = self.recorders.get_mut(name) {
            r.record_wait(params);
        }
    }

    /// Record an assert step into the named scenario recorder.
    pub fn record_scenario_assert(&mut self, name: &str, params: serde_json::Value) {
        if let Some(r) = self.recorders.get_mut(name) {
            r.record_assert(params);
        }
    }

    /// Finish recording: returns the completed scenario and stops tracking it.
    pub fn finish_scenario_recording(
        &mut self,
        name: &str,
    ) -> Option<crate::scenario::model::Scenario> {
        let rec = self.recorders.remove(name)?;
        Some(rec.build())
    }

    /// True while a recorder with this name is active.
    pub fn is_recording_scenario(&self, name: &str) -> bool {
        self.recorders.contains_key(name)
    }

    /// Names of active scenario recordings.
    pub fn active_recordings(&self) -> Vec<String> {
        let mut names: Vec<String> = self.recorders.keys().cloned().collect();
        names.sort();
        names
    }

    /// Save a completed scenario to the run's `scenarios/` directory as JSON.
    /// Save a scenario: memory first (canonical), then disk when the run is
    /// persistent. Returns the artifact path when persistence happened, so
    /// ephemeral runs report `saved_to: null` honestly but the scenario is
    /// still listed and exportable.
    pub fn save_scenario(
        &mut self,
        scenario: crate::scenario::model::Scenario,
    ) -> Option<PathBuf> {
        self.saved_scenarios
            .insert(scenario.name.clone(), scenario);
        let dir = self.run_dir.as_ref()?.join("scenarios");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}.json", sanitize(&self.saved_scenarios
            .values()
            .last()
            .expect("just inserted")
            .name)));
        // Atomic write: temp file then rename (audit item 16).
        let scenario = self.saved_scenarios.values().last().expect("just inserted");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(scenario).ok()?).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
        Some(path)
    }

    /// List saved scenarios in this run: the in-memory canonical set plus
    /// anything persisted in the run dir (deduplicated).
    pub fn list_saved_scenarios(&self) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = self.saved_scenarios.keys().cloned().collect();
        if let Some(root) = self.run_dir.as_ref() {
            let dir = root.join("scenarios");
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if !out.iter().any(|n| n == stem) {
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

    /// Load a scenario by name: memory first, then the run dir.
    pub fn load_scenario(
        &self,
        name: &str,
    ) -> anyhow::Result<crate::scenario::model::Scenario> {
        if let Some(s) = self.saved_scenarios.get(name) {
            return Ok(s.clone());
        }
        let dir = self.scenario_dir()?;
        let path = dir.join(format!("{}.json", sanitize(name)));
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("scenario '{}' not found in run '{}': {}", name, self.id, e))?;
        Ok(serde_json::from_slice(&bytes)?)
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
        };
        let tmp = dir.join("run.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, dir.join("run.json"))?;
        Ok(())
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
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
        assert!(run.begin_scenario_recording("demo"));
        assert!(!run.begin_scenario_recording("demo"), "duplicate names refused");
        run.record_scenario_act(
            "demo",
            serde_json::json!({"action": "key", "key": "enter"}),
        );
        run.record_scenario_assert("demo", serde_json::json!({"assertion": "text", "text": "OK"}));
        let scenario = run.finish_scenario_recording("demo").expect("scenario");
        assert_eq!(scenario.step_count(), 2);
        assert!(!run.is_recording_scenario("demo"));
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
        let path = run.save_scenario(scenario).expect("save");
        assert!(path.exists());

        let names = run.list_saved_scenarios().expect("list");
        assert_eq!(names, vec!["roundtrip".to_string()]);

        let loaded = run.load_scenario("roundtrip").expect("load");
        assert_eq!(loaded.step_count(), 2);
        assert_eq!(loaded.name, "roundtrip");
    }
}
