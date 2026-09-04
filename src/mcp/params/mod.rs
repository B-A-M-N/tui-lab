//! Tool parameter structs (schemas) for the MCP tools.
//!
//! Item 30: tui_act uses a tagged enum (`TuiActRequest`) so Hermes gets a
//! proper discriminated union schema instead of a flat struct with many
//! optional fields that permits nonsensical combinations.
//!
//! Wave G item 70: every action/mode/condition/assertion/profile selector is
//! a typed enum, not a `String` matched deep in a handler. Each enum pairs a
//! closed variant list (which generates a real JSON Schema `enum`) with the
//! [`Known::Other`] escape hatch: an *unknown* value still deserializes (so
//! the handler can answer `invalid_request` with the full expected list
//! through the normal envelope) instead of dying in the transport layer with
//! a bare JSON-RPC deserialization error that carries no remediation.

// Handler-family modules (review §15): split from the former monolithic
// params.rs; every public item is re-exported here so `tui_lab::mcp::params::X`
// paths are unchanged.
mod audit;
mod common;
mod contract;
mod coverage;
mod explore;
mod framework;
mod intent;
mod interact;
mod observe;
mod probe;
mod run;
mod scenario;
mod session;

pub use audit::*;
pub use common::*;
pub use contract::*;
pub use coverage::*;
pub use explore::*;
pub use framework::*;
pub use intent::*;
pub use interact::*;
pub use observe::*;
pub use probe::*;
pub use run::*;
pub use scenario::*;
pub use session::*;
