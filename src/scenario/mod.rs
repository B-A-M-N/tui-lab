//! Scenario recorder/runner/replay system (spec item 39).
//!
//! Records user interactions as deterministic scenarios that can be replayed,
//! compared, and used as regression tests.

pub mod model;
pub mod recorder;
pub mod runner;

pub use model::{Scenario, ScenarioMetadata, ScenarioStep, StepKind};
pub use recorder::ScenarioRecorder;
pub use runner::ScenarioRunner;
