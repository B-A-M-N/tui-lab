//! MCP layer: tools, params, helpers, capability registry.

pub mod helpers;
pub(crate) mod lifecycle;
pub(crate) mod ownership;
pub mod params;
pub mod registry;
pub(crate) mod resources;
pub mod tools;

pub use tools::TuiLabServer;
