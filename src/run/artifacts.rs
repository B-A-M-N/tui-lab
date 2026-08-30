//! Typed artifact references (Wave B item 15): large evidence (recordings,
//! event logs, big frames, run bundles) is registered once and handed to
//! agents as a compact ref — never inlined into tool output.
//!
//! Borrowed from the later terminal-MCP wave: a consumer that wants the
//! bytes resolves the ref (path + kind + size up front), which keeps tool
//! results token-small and makes every large artifact addressable across
//! the run's lifetime.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What an artifact holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Asciinema v3 cast (`.cast`).
    Recording,
    /// NDJSON terminal-event log.
    EventLog,
    /// NDJSON transaction ledger.
    TransactionLog,
    /// JSON state-graph export.
    StateGraph,
    /// A captured frame (canonical or rendered).
    Frame,
    /// Anything else (scenarios, findings bundles, ...).
    Other,
}

/// A compact, typed handle to one artifact in the run's root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Stable id (`art-<n>`) for cross-referencing in findings/evidence.
    pub id: String,
    pub kind: ArtifactKind,
    /// Path relative to the run root when persisted; `None` while the run
    /// is ephemeral (the bytes are held in run memory and flushed on
    /// promote).
    pub path: Option<PathBuf>,
    /// Byte size when known.
    pub size: Option<u64>,
    /// The session that produced it, when attributable.
    pub session: Option<String>,
    /// Human summary (one line).
    pub summary: String,
}

impl ArtifactRef {
    /// Short citable form (`art-3:recording`).
    pub fn cite(&self) -> String {
        let kind = match self.kind {
            ArtifactKind::Recording => "recording",
            ArtifactKind::EventLog => "event_log",
            ArtifactKind::TransactionLog => "transaction_log",
            ArtifactKind::StateGraph => "state_graph",
            ArtifactKind::Frame => "frame",
            ArtifactKind::Other => "other",
        };
        format!("{}:{}", self.id, kind)
    }
}
