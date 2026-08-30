//! Run manifest: the durable identity of a run (audit item: Run/Artifact
//! Model). Written once at run creation (and whenever a session's launch
//! spec is attached), read back to restore/replay a run.

use crate::session::state::LaunchSpec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    /// Schema tag for forward compatibility.
    pub schema: String,
    pub run_id: String,
    /// Unix millis.
    pub started_at: u64,
    /// The launch spec of the primary session (None for tool-only runs).
    /// Kept for manifest-schema v1 compatibility; `sessions` is the
    /// per-session authority (P0 fix 6).
    pub launch_spec: Option<LaunchSpec>,
    /// Launch spec per session id, so a multi-session run can recreate all
    /// of its sessions on replay (P0 fix 6).
    #[serde(default)]
    pub sessions: HashMap<String, LaunchSpec>,
    /// Which session the run treats as primary.
    #[serde(default)]
    pub primary_session: Option<String>,
    /// Replay-history completeness (P0 fix 5): false when the in-memory
    /// ledger evicted records before the flush. A manifest that says
    /// `history_complete: false` names the gap — `first_available_seq` /
    /// `dropped_records` — rather than implying replayability.
    #[serde(default = "default_true")]
    pub history_complete: bool,
    #[serde(default)]
    pub first_available_seq: Option<u64>,
    #[serde(default)]
    pub dropped_records: u64,
    /// True once `tui_run close` marked the run finished.
    #[serde(default)]
    pub closed: bool,
}

fn default_true() -> bool {
    true
}

/// Load a manifest from a run directory.
pub fn load(run_dir: &std::path::Path) -> anyhow::Result<RunManifest> {
    let bytes = std::fs::read(run_dir.join("run.json"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), LaunchSpec::new("python3", 80, 24));
        let m = RunManifest {
            schema: "tui-lab.run.v1".into(),
            run_id: "run-abc".into(),
            started_at: 1_000,
            launch_spec: Some(LaunchSpec::new("python3", 80, 24)),
            sessions,
            primary_session: Some("s1".into()),
            history_complete: false,
            first_available_seq: Some(257),
            dropped_records: 256,
            closed: false,
        };
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("run.json"),
            serde_json::to_vec_pretty(&m).expect("serialize"),
        )
        .expect("write");
        let loaded = load(dir.path()).expect("load");
        assert_eq!(loaded.run_id, "run-abc");
        assert_eq!(
            loaded.launch_spec.as_ref().map(|s| s.command.as_str()),
            Some("python3")
        );
        assert_eq!(loaded.sessions.len(), 1);
        assert!(!loaded.history_complete, "gap must survive the round-trip");
        assert_eq!(loaded.dropped_records, 256);
        assert_eq!(loaded.first_available_seq, Some(257));
    }

    /// A v1 manifest without the new fields loads with honest defaults:
    /// complete history (nothing was dropped in v1 runs), empty session map.
    #[test]
    fn v1_manifest_loads_with_defaults() {
        let v1 = serde_json::json!({
            "schema": "tui-lab.run.v1",
            "run_id": "run-old",
            "started_at": 1,
            "launch_spec": null,
            "closed": false,
        });
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("run.json"),
            serde_json::to_vec_pretty(&v1).expect("serialize"),
        )
        .expect("write");
        let loaded = load(dir.path()).expect("load");
        assert!(loaded.history_complete);
        assert!(loaded.sessions.is_empty());
        assert!(loaded.primary_session.is_none());
    }
}
