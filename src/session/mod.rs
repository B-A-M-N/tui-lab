//! Session model and manager (spec section 41). Hermes operates on named
//! sessions; sessions hold one backend + the last observed screen.

pub mod actor;
pub mod isolation;
pub mod lease;
pub mod manager;
pub mod state;

pub use actor::{ActorError, SessionActor, SessionPool};
pub use isolation::{Isolation, IsolationEvidence};
pub use lease::LeaseState;
pub use manager::SessionManager;
pub use state::{LaunchSpec, Session};
