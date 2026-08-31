//! Knowledge modes — how much structure a semantic model has been granted.
//!
//! See [`knowledge::KnowledgeMode`] for the ordering and the review's
//! "classify before debug is forbidden" guarantee.

pub mod knowledge;

pub use knowledge::KnowledgeMode;