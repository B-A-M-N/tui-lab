//! The one canonical executor for interactions (audit re-review item 4).
//!
//! MCP `tui_act`/`tui_wait`/`tui_assert`, the ScenarioRunner, the explorer,
//! and the audit drivers must never have separate interpretations of what
//! `"ctrl+c"`, `screen_stable`, or `focus` mean. They all go through
//! [`execute_act`] / [`execute_wait`] / [`execute_assert`] here, which are
//! the *only* places that translate an intent into Session operations.
//!
//! Every act runs as an [`InteractionTransaction`]: baseline event state is
//! captured *before* the input is sent, the settle wait is anchored on that
//! baseline (`wait_after`), and the outcome reports honestly whether the
//! screen actually settled (re-review items 8/9).
//!
//! Split into family files (review §15 god-object residue): the action
//! vocabulary in [`record`], the transaction record in [`transaction`], the
//! executor + its tests in [`executor`], waits/asserts in [`wait`].

mod executor;
mod record;
mod transaction;
mod wait;

pub use executor::{
    execute_act, execute_act_as, execute_act_with_completion,
    execute_act_with_completion_and_origin, execute_act_with_guard,
    execute_act_with_guard_and_origin, execute_act_with_visibility,
};
pub use record::{
    CanonicalAction, DriveOrigin, InputVisibility, ObservationAnchor, PersistedAction, SettleStatus,
};
pub use transaction::{ActionEnvelope, InteractionTransaction, RenderOp, RenderTransaction};
pub use wait::{execute_assert, execute_wait, execute_wait_event, WaitEventOutcome};

pub mod guard;
pub use guard::MutationGuard;

/// The central machine-driving pipeline (audit P0-1): one
/// execute→evidence→fold path every driving facility shares.
pub mod drive;
pub use drive::{
    act_request_json, drive as drive_pipeline, fold_session_events,
    DriveOutcome as CoreDriveOutcome, DriveSpec as CoreDriveSpec, RunTicket,
    ScenarioCapture as CoreScenarioCapture,
};
