//! Checkpoint store: manages checkpoints per session, with persistent
//! storage to the run directory.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::model::{Checkpoint, CheckpointComparison, CheckpointMatches, FocusDiff, ScreenHashes};
use crate::error::{Envelope, ErrorCategory};

pub struct CheckpointStore {
    /// Session ID -> checkpoints by name.
    checkpoints: HashMap<String, HashMap<String, Checkpoint>>,
    /// Sequence counter for ordering.
    next_seq: u64,
    /// Optional run directory for persistent storage.
    run_dir: Option<String>,
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
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
