//! Session model and manager (spec section 41). Hermes operates on named
//! sessions; sessions hold one backend + the last observed screen.
//!
//! The live session surface is the actor-backed [`SessionPool`]: one OS
//! thread per session with a bounded mailbox, so session I/O never blocks
//! the async runtime or any other session. The legacy synchronous
//! [`SessionManager`] held one global `Mutex` around EVERY session and its
//! blocking path duplicated the actor's job without per-session concurrency.
//! It is declared **legacy / do-not-use** (review-2 structural item) and kept
//! only to keep the integration test suite green — new code must use
//! [`SessionPool`] (or own a single [`Session`] directly).

pub mod actor;
pub mod isolation;
pub mod lease;
pub mod locator;
pub mod manager;
pub mod scratch;
pub mod state;

pub use actor::{ActorError, SessionActor, SessionPool};
pub use isolation::{Isolation, IsolationEvidence};
pub use lease::LeaseState;
pub use locator::ProjectLocator;
pub use manager::SessionManager;
pub use scratch::{EnvironmentPolicy, ScratchDir};
pub use state::{FrameAnalysis, LaunchSpec, Session};

#[cfg(test)]
mod retirement_gate_tests {
    /// Review P2 (SessionManager retirement assessment): production must
    /// never construct or hold a `SessionManager` again — the actor-backed
    /// [`super::SessionPool`] is the live surface, and the legacy manager is
    /// test-only. This gate scans every production module for the type name
    /// so a future import (in an `impl`, a struct field, a constructor call)
    /// fails the build instead of silently reintroducing global-Mutex
    /// serialization. Allowed references: the module's own definition, the
    /// re-export, and doc comments.
    #[test]
    fn production_never_uses_session_manager() {
        let mut violations: Vec<String> = Vec::new();
        let prod_roots = ["src/mcp", "src/run", "src/audit", "src/execution"];
        for root in prod_roots {
            let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            dir.push(root);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (ln, line) in src.lines().enumerate() {
                    if line.contains("SessionManager") && !line.trim_start().starts_with("//") {
                        violations.push(format!("{}:{}", path.display(), ln + 1));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "production modules must not reference SessionManager (legacy, \
             test-only):\n{}",
            violations.join("\n")
        );
    }
}
