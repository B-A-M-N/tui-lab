//! Artifact registration and capture holds.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Wave F item 57: retain a screen capture (SVG/PNG bytes) in run
    /// memory while ephemeral; `flush` writes it under `captures/`.
    pub fn hold_capture(&mut self, file_name: String, body: Vec<u8>, format: &str) {
        self.held_captures
            .push((file_name, body, format.to_string()));
    }

    /// Register a produced artifact and get its typed ref (Wave B item 15).
    pub fn register_artifact(
        &mut self,
        kind: ArtifactKind,
        path: Option<PathBuf>,
        size: Option<u64>,
        session: Option<String>,
        summary: impl Into<String>,
    ) -> anyhow::Result<ArtifactRef> {
        self.ensure_open()?;
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
        Ok(r)
    }

    /// All artifacts registered in this run.
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }
}
