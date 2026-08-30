//! Run manifest: the durable identity of a run (audit item: Run/Artifact
//! Model). Written once at run creation (and whenever the launch spec is
//! attached), read back to restore/replay a run.

use crate::session::state::LaunchSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    /// Schema tag for forward compatibility.
    pub schema: String,
    pub run_id: String,
    /// Unix millis.
    pub started_at: u64,
    /// The launch spec of the primary session (None for tool-only runs).
    pub launch_spec: Option<LaunchSpec>,
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
        let m = RunManifest {
            schema: "tui-lab.run.v1".into(),
            run_id: "run-abc".into(),
            started_at: 1_000,
            launch_spec: Some(LaunchSpec::new("python3", 80, 24)),
        };
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(
            dir.path().join("run.json"),
            serde_json::to_vec_pretty(&m).expect("serialize"),
        )
        .expect("write");
        let loaded = load(dir.path()).expect("load");
        assert_eq!(loaded.run_id, "run-abc");
        assert_eq!(loaded.launch_spec.as_ref().map(|s| s.command.as_str()), Some("python3"));
    }
}
