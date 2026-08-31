//! Shared small types re-exported by the backend modules (kept here so the
//! `line_cli` and `portable_pty` modules can both `use` them without a
//! circular re-export through `mod.rs`).

pub use super::{CommandState, SearchHit};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
