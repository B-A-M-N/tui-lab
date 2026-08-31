//! Checkpoint store: manages checkpoints per session, with persistent
//! storage to the run directory.

use std::collections::HashMap;
use std::fs;

use super::model::{Checkpoint, CheckpointComparison, CheckpointMatches, FocusDiff, ScreenHashes};
use crate::error::{Envelope, ErrorCategory};

pub struct CheckpointStore {
    /// Session ID -> checkpoints by name.
    checkpoints: HashMap<String, HashMap<String, Checkpoint>>,
    /// Sequence counter for ordering.
    next_seq: u64,
    /// Optional run directory for persistent storage. Read once persistence
    /// lands (Wave 4); dead_code is deliberate for now.
    #[allow(dead_code)]
    run_dir: Option<String>,
}

impl Default for CheckpointStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckpointStore {
    pub fn new() -> Self {
        CheckpointStore {
            checkpoints: HashMap::new(),
            next_seq: 1,
            run_dir: None,
        }
    }

    pub fn with_run_dir(run_dir: String) -> Self {
        let _ = fs::create_dir_all(&run_dir);
        CheckpointStore {
            checkpoints: HashMap::new(),
            next_seq: 1,
            run_dir: Some(run_dir),
        }
    }

    /// Re-root this store onto a new run directory, carrying over every
    /// in-memory checkpoint (run promotion: the SAME run gains durable
    /// storage; nothing accumulated while ephemeral may be lost).
    pub fn reroot(&mut self, run_dir: String) {
        let _ = fs::create_dir_all(&run_dir);
        self.run_dir = Some(run_dir);
        // Persist every carried-over checkpoint into the new root.
        let session_ids: Vec<String> = self.checkpoints.keys().cloned().collect();
        for sid in session_ids {
            let names: Vec<String> = self
                .checkpoints
                .get(&sid)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            for name in names {
                let cp = self
                    .checkpoints
                    .get(&sid)
                    .and_then(|m| m.get(&name))
                    .cloned();
                if let Some(cp) = cp {
                    if let Err(e) = self.persist(&sid, &cp) {
                        eprintln!(
                            "tui-lab: checkpoint re-persist failed for '{}': {}",
                            name, e
                        );
                    }
                }
            }
        }
    }

    /// Total checkpoints across all sessions.
    pub fn count(&self) -> usize {
        self.checkpoints.values().map(|m| m.len()).sum()
    }

    /// Save a checkpoint. Returns the checkpoint's name.
    pub fn save(
        &mut self,
        session_id: &str,
        generation: u32,
        name: Option<String>,
        screen: &crate::screen::ScreenState,
        semantic: Option<&crate::semantic::SemanticScreen>,
    ) -> String {
        let name = name.unwrap_or_else(|| format!("cp-{}", self.next_seq));
        let cp = Checkpoint {
            id: format!("checkpoint-{}-{}", session_id, self.next_seq),
            name: name.clone(),
            session_id: session_id.to_string(),
            generation,
            sequence: self.next_seq,
            screen_hashes: ScreenHashes {
                raw: screen.raw_hash.clone(),
                visual: screen.visual_hash.clone(),
                structure: screen.structure_hash.clone(),
            },
            screen_snapshot: Some(serde_json::to_value(screen).unwrap_or_default()),
            semantic_snapshot: semantic.map(|s| serde_json::to_value(s).unwrap_or_default()),
            focus: semantic.and_then(|s| s.focus.control.clone()),
            process: Some(serde_json::to_value(&screen.process).unwrap_or_default()),
            coverage: None,
            created_at: now_millis(),
        };
        self.next_seq += 1;
        if let Err(e) = self.persist(session_id, &cp) {
            eprintln!(
                "tui-lab: checkpoint persistence failed for '{}': {}",
                name, e
            );
        }
        self.checkpoints
            .entry(session_id.to_string())
            .or_default()
            .insert(name.clone(), cp);
        name
    }

    /// Compare current state against a saved checkpoint.
    pub fn compare(
        &self,
        session_id: &str,
        name: &str,
        screen: &crate::screen::ScreenState,
        semantic: Option<&crate::semantic::SemanticScreen>,
    ) -> Result<String, ErrorCategory> {
        let session_cps = self
            .checkpoints
            .get(session_id)
            .ok_or(ErrorCategory::InvalidRequest)?;
        let cp = session_cps.get(name).ok_or(ErrorCategory::InvalidRequest)?;

        let matches = CheckpointMatches {
            raw: cp.screen_hashes.raw == screen.raw_hash,
            visual: cp.screen_hashes.visual == screen.visual_hash,
            structure: cp.screen_hashes.structure == screen.structure_hash,
        };

        let focus_diff = if let Some(sem) = semantic {
            if cp.focus != sem.focus.control {
                Some(FocusDiff {
                    before: cp.focus.clone(),
                    after: sem.focus.control.clone(),
                })
            } else {
                None
            }
        } else {
            None
        };

        let comp = CheckpointComparison {
            checkpoint_name: name.to_string(),
            matches,
            screen_diff: Some(serde_json::json!({
                "before_structure": cp.screen_hashes.structure,
                "after_structure": screen.structure_hash,
            })),
            semantic_diff: Some(serde_json::json!({
                "note": "semantic diff requires full analysis comparison",
            })),
            focus_diff,
        };

        Ok(Envelope::ok(serde_json::json!({ "comparison": comp })).to_json())
    }

    /// List all checkpoint names for a session.
    pub fn list(&self, session_id: &str) -> Vec<String> {
        self.checkpoints
            .get(session_id)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Delete a checkpoint by name.
    pub fn delete(&mut self, session_id: &str, name: &str) -> bool {
        self.checkpoints
            .get_mut(session_id)
            .map(|m| m.remove(name).is_some())
            .unwrap_or(false)
    }

    /// Check if a checkpoint exists.
    pub fn contains(&self, session_id: &str, name: &str) -> bool {
        self.checkpoints
            .get(session_id)
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Persist one checkpoint to `<run_dir>/<session_id>/<name>.json`.
    ///
    /// Called by `save()` when a run dir is configured; failures are
    /// non-fatal (the in-memory copy remains authoritative) but are
    /// returned so the caller can report degraded persistence honestly.
    fn persist(&self, session_id: &str, cp: &Checkpoint) -> std::io::Result<()> {
        let Some(dir) = &self.run_dir else {
            return Ok(()); // ephemeral run: nothing to do
        };
        let dir = std::path::Path::new(dir).join(sanitize_segment(session_id));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", sanitize_segment(&cp.name)));
        let tmp = dir.join(format!(
            "{}.json.tmp-{}",
            sanitize_segment(&cp.name),
            std::process::id()
        ));
        let json = serde_json::to_vec_pretty(cp)?;
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path) // atomic
    }

    /// Load all persisted checkpoints for a session from the run dir into
    /// memory. Idempotent; unknown/corrupt files are skipped.
    pub fn load_session(&mut self, session_id: &str) -> usize {
        let Some(dir) = &self.run_dir else {
            return 0;
        };
        let dir = std::path::Path::new(dir).join(sanitize_segment(session_id));
        let mut loaded = 0;
        let Ok(entries) = fs::read_dir(&dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(cp) = serde_json::from_slice::<Checkpoint>(&bytes) else {
                continue;
            };
            // Keep the sequence counter ahead of anything loaded.
            self.next_seq = self.next_seq.max(cp.sequence + 1);
            self.checkpoints
                .entry(session_id.to_string())
                .or_default()
                .insert(cp.name.clone(), cp);
            loaded += 1;
        }
        loaded
    }
}

/// Filesystem-safe segment: alphanumerics, `-`, `_`, `.`; everything else
/// becomes `_`. Bounded to 80 chars.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> crate::screen::ScreenState {
        crate::screen::ScreenState {
            cols: 10,
            rows: 3,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: vec!["hi".to_string(), "".to_string(), "".to_string()],
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: "r1".into(),
            visual_hash: "v1".into(),
            structure_hash: "s1".into(),
            process: crate::screen::ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    #[test]
    fn checkpoint_save_persists_and_loads_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("tui-lab-cp-test-{}", uuid::Uuid::new_v4().simple()));
        let session = "sess-1";
        let name = {
            let mut store = CheckpointStore::with_run_dir(dir.to_string_lossy().to_string());
            store.save(session, 0, Some("before".into()), &screen(), None)
        };
        // Fresh store over the same dir must see the persisted checkpoint.
        let mut store2 = CheckpointStore::with_run_dir(dir.to_string_lossy().to_string());
        assert_eq!(store2.load_session(session), 1, "one checkpoint loaded");
        assert!(store2.contains(session, &name));
        let list = store2.list(session);
        assert_eq!(list, vec!["before".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_ephemeral_store_skips_persistence() {
        let mut store = CheckpointStore::new();
        let n = store.save("s", 0, None, &screen(), None);
        assert!(store.contains("s", &n));
        assert_eq!(store.load_session("s"), 0, "nothing on disk to load");
    }

    #[test]
    fn checkpoint_delete_is_real() {
        let mut store = CheckpointStore::new();
        let n = store.save("s", 0, Some("x".into()), &screen(), None);
        assert!(store.delete("s", &n));
        assert!(!store.delete("s", &n), "second delete is false");
        assert!(!store.contains("s", &n));
    }

    #[test]
    fn sanitize_segment_removes_path_chars() {
        // `.` and `/`: dots are kept (harmless inside a single segment,
        // no separators), slashes become underscores.
        assert_eq!(sanitize_segment("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_segment(""), "unnamed");
        assert_eq!(sanitize_segment("ok-name_1.json"), "ok-name_1.json");
    }
}
