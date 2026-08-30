//! Session model and manager (spec section 41). Hermes operates on named
//! sessions; sessions hold one backend + the last observed screen.

pub mod manager;
pub mod state;

pub use manager::SessionManager;
pub use state::Session;
