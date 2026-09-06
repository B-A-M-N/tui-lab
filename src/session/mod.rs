//! Session model and manager (spec section 41). Hermes operates on named
//! sessions; sessions hold one backend + the last observed screen.
//!
//! The live session surface is the actor-backed [`SessionPool`]: one OS
//! thread per session with a bounded mailbox, so session I/O never blocks
//! the async runtime or any other session. The legacy synchronous
//! `SessionManager` held one global `Mutex` around EVERY session and its
//! blocking path duplicated the actor's job without per-session concurrency.
//! It is RETIRED (audit P1-54): the module is `#[cfg(test)]` — production
//! code cannot name the type, so accidental reintroduction is a compile
//! error, not a convention. New code must use [`SessionPool`] (or own a
//! single [`Session`] directly).

pub mod actor;
pub(crate) mod event_state;
pub mod isolation;
pub mod lease;
pub mod locator;
pub(crate) mod observation;
pub mod scratch;
pub mod state;

/// Legacy global-Mutex manager, test-only. Audit P1-54: it used to stay
/// publicly exported "for the integration tests"; those have migrated to
/// [`SessionPool`], so the type is now unreachable from production builds.
#[cfg(test)]
pub mod manager;

pub use actor::{ActorError, SessionActor, SessionPool};
pub use isolation::{Isolation, IsolationEvidence};
pub use lease::LeaseState;
pub use locator::ProjectLocator;
pub use scratch::{EnvironmentPolicy, ScratchDir};
pub use state::{FrameAnalysis, LaunchSpec, Session};

#[cfg(test)]
mod retirement_gate_tests {
    /// Audit P1-54: the retirement is now ENFORCED BY THE COMPILER —
    /// `mod manager` is `#[cfg(test)]`, so any production import of
    /// `SessionManager` fails to build. This test pins the second half: the
    /// public re-export is gone, so even test-crate users reach the type
    /// only via the explicit `session::manager` path, and a production
    /// build (`cargo build`) literally cannot name it.
    #[test]
    fn session_manager_is_not_publicly_exported() {
        // The re-export was removed from this module; if someone re-adds it,
        // this compile-time-checked assertion chain breaks only via the
        // source scan below — so keep both layers: the cfg gate is the
        // enforcement, this scan documents the policy.
        let src = include_str!("mod.rs");
        // The needle is assembled so this assertion cannot match its own
        // source text.
        let reexport = format!("pub use manager::{}Manager", "Session");
        assert!(
            !src.contains(&reexport),
            "the legacy manager re-export must not return"
        );
        assert!(
            src.contains("#[cfg(test)]\npub mod manager"),
            "the legacy manager module stays test-gated"
        );
    }
}
