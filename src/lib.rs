//! hermes-tui-lab — agent-native TUI instrumentation/testing/exploration/UX harness.
//!
//! Library crate. The binary (`src/main.rs`) only wires logging + MCP transport.

pub mod audit;
pub mod backend;
pub mod checkpoint;
pub mod coverage;
pub mod design;
pub mod error;
pub mod exploration;
pub mod framework;
pub mod mcp;
pub mod recording;
pub mod run;
pub mod scenario;
pub mod screen;
pub mod semantic;
pub mod session;

pub const SKILL_DOC: &str = include_str!("../SKILL.md");
