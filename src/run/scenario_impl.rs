//! Scenario recording, saved scenarios, and held recordings.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
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
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_act(params.clone());
            }
        }
        Ok(())
    }

    /// Record a SENSITIVE act step into matching session-generation
    /// recordings (re-review P0.3): the payload field becomes a `${NAME}`
    /// reference and the parameter is declared on the scenario. The secret
    /// never reaches the scenario file.
    pub fn record_scenario_act_sensitive(
        &mut self,
        session_id: &str,
        generation: u32,
        action_params: serde_json::Value,
        payload_field: &str,
        kind: crate::scenario::model::SensitiveKind,
        byte_len: usize,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_act_sensitive(action_params.clone(), payload_field, kind, byte_len);
            }
        }
        Ok(())
    }

    /// Record a wait step into matching session-generation recordings.
    pub fn record_scenario_wait(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_wait(params.clone());
            }
        }
        Ok(())
    }

    /// Record an assert step into matching session-generation recordings.
    pub fn record_scenario_assert(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.recorders.values_mut() {
            if r.matches(session_id, generation) {
                r.record_assert(params.clone());
            }
        }
        Ok(())
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

    /// Look up a recording id by name (any session). Duplicate names are an
    /// **error**, not an oldest-match fallback (re-review item 45): an
    /// ambiguous name silently stopping the wrong recording is worse than
    /// refusing. `Err(names)` lists every conflicting id so the caller can
    /// disambiguate by id; callers that care about identity hold the id
    /// from `begin_scenario_recording` in the first place.
    pub fn find_recording_by_name(&self, name: &str) -> Result<String, Vec<String>> {
        let matches: Vec<String> = self
            .recorders
            .values()
            .filter(|r| r.name == name)
            .map(|r| r.id.as_str().to_string())
            .collect();
        match matches.len() {
            0 => Err(Vec::new()),
            1 => Ok(matches.into_iter().next().expect("one match")),
            _ => {
                let mut ids = matches;
                ids.sort();
                Err(ids)
            }
        }
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
        let payload = serde_json::to_value(&scenario).ok()?;
        std::fs::write(
            &tmp,
            crate::run::formats::Envelope::wrap(crate::run::formats::tags::SCENARIO, payload)
                .to_vec_pretty()
                .ok()?,
        )
        .ok()?;
        std::fs::rename(&tmp, &path).ok()?;
        Some(path)
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
                self.id()
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
            self.id()
        ))
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
}
