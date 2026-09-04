//! Active audit drivers: each drives the application through a session to
//! perform its audit family (spec items 52-55 and the Wave 3c readers).
//!
//! Formerly one 3,669-line `driver.rs` (review §15 follow-up, god-object
//! residue): the families now live in sibling files, one per family, with
//! the shared evidence/decode helpers in [`shared`]. This module keeps the
//! `crate::audit::driver::*` paths the orchestrator's descriptor table
//! uses — every family's `pub` entry point is re-exported at this level —
//! so the table reads exactly as before.

mod interaction;
mod keyboard;
mod layout;
mod lifecycle;
mod protocol;
mod resize;
mod shared;
mod shell_cli;
mod states;
mod visual;

pub use interaction::{mouse_audit, performance_audit};
pub use keyboard::{focus_audit, keyboard_audit};
pub use layout::{clipping_audit, navigation_audit, navigation_keys_audit};
pub use lifecycle::{lifecycle_audit, lifecycle_exit_audit};
pub use protocol::{input_protocol_audit, query_response_audit, terminal_modes_audit};
pub use resize::{resize_audit, resize_reflow_audit};
pub use shell_cli::shell_cli_audit;
pub use states::{errors_audit, states_audit};
pub use visual::{color_audit, rendering_audit};
