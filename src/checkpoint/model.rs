//! Checkpoint model (spec item 38).

use serde::{Deserialize, Serialize};

/// A named checkpoint capturing full UI state at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub name: String,
    pub session_id: String,
    pub generation: u32,
    pub sequence: u64,
    /// Raw, visual, and structure hashes of the screen.
    pub screen_hashes: ScreenHashes,
    /// Full screen snapshot (cells + viewport text).
    pub screen_snapshot: Option<serde_json::Value>,
    /// Semantic analysis at checkpoint time.
    pub semantic_snapshot: Option<serde_json::Value>,
    /// Focused control, if any.
    pub focus: Option<String>,
    /// Process state.
    pub process: Option<serde_json::Value>,
    /// Coverage snapshot, if available.
    pub coverage: Option<serde_json::Value>,
    /// Creation timestamp (unix millis).
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenHashes {
    pub raw: String,
    pub visual: String,
    pub structure: String,
}

/// Result of comparing a checkpoint against current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointComparison {
    pub checkpoint_name: String,
    pub matches: CheckpointMatches,
    pub screen_diff: Option<serde_json::Value>,
    pub semantic_diff: Option<serde_json::Value>,
    pub focus_diff: Option<FocusDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMatches {
    pub raw: bool,
    pub visual: bool,
    pub structure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusDiff {
    pub before: Option<String>,
    pub after: Option<String>,
}
