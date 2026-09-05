//! Cohesive state for the scenario domain.
//!
//! Round-2 decomposition (G1): the three `RunContext` fields that make up
//! scenario state — in-progress recorders (session+generation scoped), the
//! id-keyed saved set, and the unambiguous-name index — move OUT of the
//! flat run bucket into this holder. The name-index invariant is STORE
//! policy, not run lifecycle policy: a name maps to exactly one scenario
//! or resolves through neither.
//!
//! Filesystem writes (the `<name>-<id-suffix>.json` artifacts) and run-dir
//! path resolution stay on the `RunContext` facade — this store is state,
//! not IO. Identity remains ID-based, not name-based (re-review Wave-1
//! item 9): names are display metadata and may collide.

use std::collections::HashMap;

/// Scenario recorders + saved set + name index for one run.
#[derive(Default)]
pub(super) struct ScenarioStore {
    /// In-progress scenario recordings by opaque id (re-review item 5:
    /// session+generation scoped; names are not identities).
    recorders: HashMap<String, super::recording_scope::ScenarioRecording>,
    /// Completed scenarios for this run, keyed by [`crate::scenario::model::Scenario::id`]
    /// (re-review Wave-1 item 9: names are display metadata and may
    /// collide; ids are the storage key). Memory is canonical during the
    /// live run (so ephemeral runs still list/export); persistence to the
    /// run dir is an artifact layer on top, not the source of truth. The
    /// secondary name index only maps *unambiguous* names — a name held by
    /// two scenarios resolves through neither.
    saved: HashMap<String, crate::scenario::model::Scenario>,
    /// Unambiguous-name → id index (rebuilt on every mutation).
    names: HashMap<String, String>,
}

impl ScenarioStore {
    /// A fresh, empty store.
    pub(super) fn new() -> Self {
        ScenarioStore::default()
    }

    /// Start recording a scenario bound to one session generation. Returns
    /// the recording id. Names are display labels, not identities —
    /// parallel recordings may share a name.
    pub(super) fn begin_recording(
        &mut self,
        name: &str,
        session_id: &str,
        generation: u32,
    ) -> super::recording_scope::ScenarioRecordingId {
        let rec = super::recording_scope::ScenarioRecording::new(name, session_id, generation);
        let id = rec.id.clone();
        self.recorders.insert(id.as_str().to_string(), rec);
        id
    }

    /// Mutable access to every recording scoped to this exact session
    /// generation (re-review item 5: session A's recording never absorbs
    /// session B's traffic).
    pub(super) fn recorders_for(
        &mut self,
        session_id: &str,
        generation: u32,
    ) -> Vec<&mut super::recording_scope::ScenarioRecording> {
        self.recorders
            .values_mut()
            .filter(|r| r.matches(session_id, generation))
            .collect()
    }

    /// Finish a recording by id: returns the completed scenario and stops
    /// tracking it.
    pub(super) fn finish_recording(
        &mut self,
        recording_id: &str,
    ) -> Option<crate::scenario::model::Scenario> {
        let mut rec = self.recorders.remove(recording_id)?;
        Some(rec.finish())
    }

    /// Recording ids by name (any session); ambiguous names return every
    /// conflicting id (re-review item 45: an ambiguous name silently
    /// stopping the wrong recording is worse than refusing).
    pub(super) fn recording_ids_by_name(&self, name: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .recorders
            .values()
            .filter(|r| r.name == name)
            .map(|r| r.id.as_str().to_string())
            .collect();
        ids.sort();
        ids
    }

    /// Recording metadata for status/display, sorted by id.
    pub(super) fn active_recording_meta(&self) -> Vec<serde_json::Value> {
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

    /// True while any recording is active for this exact session
    /// generation.
    pub(super) fn is_recording(&self, session_id: &str, generation: u32) -> bool {
        self.recorders
            .values()
            .any(|r| r.matches(session_id, generation))
    }

    /// Save a scenario into memory (the canonical live-run layer). Storage
    /// key is the scenario's *id*; same-named scenarios from different
    /// sessions coexist. The name index is rebuilt after every mutation.
    pub(super) fn save(&mut self, scenario: crate::scenario::model::Scenario) {
        self.names.remove(&scenario.name);
        self.saved.insert(scenario.id.clone(), scenario.clone());
        self.rebuild_name_index();
    }

    /// The saved scenario ids.
    pub(super) fn ids(&self) -> Vec<String> {
        self.saved.keys().cloned().collect()
    }

    /// The saved set (id → scenario), for iteration.
    pub(super) fn all(&self) -> &HashMap<String, crate::scenario::model::Scenario> {
        &self.saved
    }

    /// How many scenarios are saved.
    pub(super) fn len(&self) -> usize {
        self.saved.len()
    }

    /// Adopt a loaded scenario (restore path).
    pub(super) fn insert_loaded(&mut self, scenario: crate::scenario::model::Scenario) {
        self.saved.insert(scenario.id.clone(), scenario);
    }

    /// Rebuild the unambiguous-name index over the in-memory set (after a
    /// bulk restore). A name that maps to exactly one scenario resolves; a
    /// name held by two or more resolves through neither (callers must
    /// use the id).
    pub(super) fn rebuild_name_index(&mut self) {
        self.names.clear();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for sc in self.saved.values() {
            *counts.entry(sc.name.as_str()).or_insert(0) += 1;
        }
        for sc in self.saved.values() {
            if counts.get(sc.name.as_str()).copied().unwrap_or(0) == 1 {
                self.names.insert(sc.name.clone(), sc.id.clone());
            }
        }
    }

    /// Resolve a saved scenario by id (preferred) or unambiguous name.
    /// `Resolved::Id` / `Resolved::Name` / ambiguous / absent.
    pub(super) fn resolve(
        &self,
        key: &str,
    ) -> Result<&crate::scenario::model::Scenario, ResolveError> {
        if let Some(s) = self.saved.get(key) {
            return Ok(s);
        }
        if let Some(id) = self.names.get(key) {
            if let Some(s) = self.saved.get(id) {
                return Ok(s);
            }
        }
        if self.saved.values().any(|sc| sc.name == key) {
            // Multiple scenarios carry this name and the name index already
            // refused it: never guess between them.
            return Err(ResolveError::AmbiguousName);
        }
        Err(ResolveError::Absent)
    }
}

/// Why a scenario key did not resolve to an in-memory scenario.
pub(super) enum ResolveError {
    /// The name is held by two or more scenarios — never guess.
    AmbiguousName,
    /// No id or name match in memory (the run-dir fallback may still find
    /// it on disk).
    Absent,
}
