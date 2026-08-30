//! Screen module: cell model, state extraction, diffing, normalization.

pub mod cell;
pub mod diff;
pub mod normalize;
pub mod state;

pub use cell::{Cell, Color, CursorState, ProcessState, ScreenState};
pub use diff::{diff, Transition};
pub use state::from_vt;
