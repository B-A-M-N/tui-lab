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
