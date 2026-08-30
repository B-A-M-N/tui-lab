//! Persistent, session-scoped checkpoints (spec item 38).
//!
//! Replaces the old process-global `OnceLock<Mutex<HashMap<String, String>>>`
//! with proper checkpoint objects tied to a specific session/generation.

pub mod model;
pub mod store;

pub use model::{Checkpoint, CheckpointComparison};
pub use store::CheckpointStore;
