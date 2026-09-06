//! MCP layer: tools, params, helpers, capability registry.

pub mod helpers;
pub(crate) mod ownership;
pub mod params;
pub(crate) mod resources;
pub mod registry;
pub mod tools;

pub use tools::TuiLabServer;
