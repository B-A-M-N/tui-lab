//! Handler families (review §1/§5): each `#[tool]` method's body lives
//! here as a free function over `&TuiLabServer`; the methods in `super`
//! stay only as rmcp-registered thin delegates. Child modules of `tools`,
//! so the struct's private fields need no visibility changes.

pub(crate) mod audit;
pub(crate) mod contract;
pub(crate) mod coverage;
pub(crate) mod drive;
pub(crate) mod explore;
pub(crate) mod framework;
pub(crate) mod interact;
pub(crate) mod observe;
pub(crate) mod observe_modes;
pub(crate) mod probe;
pub(crate) mod run;
pub(crate) mod scenario;
pub(crate) mod session;
