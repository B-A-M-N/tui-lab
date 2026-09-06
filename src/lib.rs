//! tui-lab — agent-native TUI instrumentation/testing/exploration/UX harness.
//!
//! Library crate. The binary (`src/main.rs`) only wires logging + MCP transport.

pub mod audit;
pub mod backend;
pub mod capture;
pub mod checkpoint;
pub mod coverage;
pub mod design;
pub mod diagnostic;
pub mod error;
pub mod events;
pub mod execution;
pub mod exploration;
pub mod framework;
pub mod intent;
pub mod mcp;
pub mod mode;
pub mod protocol;
pub mod recording;
pub mod run;
pub mod scenario;
pub mod screen;
pub mod semantic;
pub mod session;
pub mod terminal;

pub const SKILL_DOC: &str = include_str!("../SKILL.md");
