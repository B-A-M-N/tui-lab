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
        self.scenarios.begin_recording(name, session_id, generation)
    }

    /// Record an act step into every recording scoped to this exact session
    /// generation (re-review item 5: session A's recording never absorbs
    /// session B's traffic).
    /// Beta-audit P0-9: record a first-class intent step (target + verb
    /// as semantic facts) into every active scenario recording for the
    /// session. Replay re-resolves the target and re-runs the
    /// focus-secured plan.
    pub fn record_scenario_intent(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_intent(params.clone());
        }
        Ok(())
    }

    pub fn record_scenario_act(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_act(params.clone());
        }
        Ok(())
    }

    /// Record an act step with a replay precondition into every matching
    /// session-generation recording. The ordinary live recording path uses
    /// this so the exact pre-dispatch frame becomes the replay guard.
    pub fn record_scenario_act_with_expect(
        &mut self,
        session_id: &str,
        generation: u32,
        params: serde_json::Value,
        expect: crate::scenario::model::StepExpect,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_act_with_expect(params.clone(), expect.clone());
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
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_act_sensitive(action_params.clone(), payload_field, kind, byte_len);
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
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_wait(params.clone());
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
        for r in self.scenarios.recorders_for(session_id, generation) {
            r.record_assert(params.clone());
        }
        Ok(())
    }

    /// Current recorded step count for one recording id. `None` when the
    /// recording does not exist. Used to refuse an empty record_stop
    /// without consuming the in-progress recorder.
    pub fn scenario_recording_step_count(&self, recording_id: &str) -> Option<usize> {
        self.scenarios.recording_step_count(recording_id)
    }

    /// Finish a recording by id: returns the completed scenario and stops
    /// tracking it.
    pub fn finish_scenario_recording(
        &mut self,
        recording_id: &str,
    ) -> Option<crate::scenario::model::Scenario> {
        self.scenarios.finish_recording(recording_id)
    }

    /// Look up a recording id by name (any session). Duplicate names are an
    /// **error**, not an oldest-match fallback (re-review item 45): an
    /// ambiguous name silently stopping the wrong recording is worse than
    /// refusing. `Err(names)` lists every conflicting id so the caller can
    /// disambiguate by id; callers that care about identity hold the id
    /// from `begin_scenario_recording` in the first place.
    pub fn find_recording_by_name(&self, name: &str) -> Result<String, Vec<String>> {
        match self.scenarios.recording_ids_by_name(name).as_slice() {
            [] => Err(Vec::new()),
            [one] => Ok(one.clone()),
            ids => Err(ids.to_vec()),
        }
    }

    /// Metadata for all active recordings.
    pub fn active_recordings(&self) -> Vec<serde_json::Value> {
        self.scenarios.active_recording_meta()
    }

    /// True while any recording is active for this exact session generation.
    pub fn is_recording_scenario(&self, session_id: &str, generation: u32) -> bool {
        self.scenarios.is_recording(session_id, generation)
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
        self.scenarios.save(scenario.clone());
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
        let mut out: Vec<String> = self.scenarios.ids();
        // Stems this run already accounts for in memory.
        let known_stems: Vec<String> = self
            .scenarios
            .all()
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
        // 1+2. Direct id hit, then the unambiguous-name fallback — both
        // store policy now (the name index refuses ambiguity itself).
        match self.scenarios.resolve(key) {
            Ok(s) => return Ok(s.clone()),
            Err(scenario_store::ResolveError::AmbiguousName) => {
                return Err(anyhow::anyhow!(
                    "scenario name '{}' is ambiguous in run '{}' — pass scenario_id instead",
                    key,
                    self.id()
                ));
            }
            Err(scenario_store::ResolveError::Absent) => {}
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
        self.artifacts_store.hold_recording(file_name, ndjson);
    }

    /// Recordings held in memory (name + event count preview).
    pub fn held_recordings(&self) -> Vec<serde_json::Value> {
        self.artifacts_store
            .held_recordings()
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
