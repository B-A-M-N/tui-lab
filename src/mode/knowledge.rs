//! KnowledgeMode — how much structure a semantic model has been granted
//! (W2.8, review: "must not classify the UI before it can debug the UI").
//!
//! The mode expresses *how the semantic tree was informed*, not *how well it
//! was informed*. The ordering is cumulative:
//!
//!   Raw            — nothing but the raw screen bytes; the debugger can
//!                    still scan text, diff frames, and emit diagnostics.
//!   Inferred       — structure was inferred from the rendered output
//!                    (regions/controls/recognizers), no app cooperation.
//!   NativeEnriched — the app declared its own tree over the native channel
//!                    (framework adapters / NativeChannel), upgrading focus,
//!                    roles, and affordances that inference could only guess.
//!   Contracted     — the app additionally honored a contract (a declared
//!                    widget/behavior surface) constraining what it will do.
//!
//! Invariants (frozen): a mode never *requires* a higher one — Raw works with
//! zero contract/source/native. `from_evidence` only promotes to a mode whose
//! evidence is actually present; it never claims a capability the backend
//! hasn't demonstrated (the same honesty rule as `Capabilities`).

/// The knowledge mode of a semantic tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum KnowledgeMode {
    Raw,
    Inferred,
    NativeEnriched,
    Contracted,
}

impl KnowledgeMode {
    /// Choose the strongest mode the *present evidence* justifies.
    ///
    /// `has_contract` implies a native declaration (a contract lives on top of
    /// the native tree), which implies inferred structure. Each input only
    /// promotes the mode when it is genuinely available, so a bare raw screen
    /// that inference happened to leave alone stays `Raw` — never `Contracted`.
    pub fn from_evidence(structure_inferred: bool, has_native: bool, has_contract: bool) -> Self {
        if has_contract {
            KnowledgeMode::Contracted
        } else if has_native {
            KnowledgeMode::NativeEnriched
        } else if structure_inferred {
            KnowledgeMode::Inferred
        } else {
            KnowledgeMode::Raw
        }
    }

    /// Whether the mode carried app-provided structure (native or contract).
    pub fn has_declared_structure(self) -> bool {
        matches!(self, Self::NativeEnriched | Self::Contracted)
    }

    /// Whether the mode imposes a contract the app must have honored.
    pub fn requires_contract(self) -> bool {
        self == Self::Contracted
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Inferred => "inferred",
            Self::NativeEnriched => "native_enriched",
            Self::Contracted => "contracted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_needs_no_evidence() {
        // The review's headline: Raw must never require contract/source/native.
        assert_eq!(KnowledgeMode::from_evidence(false, false, false), KnowledgeMode::Raw);
    }

    #[test]
    fn evidence_promotes_cumulatively() {
        assert_eq!(KnowledgeMode::from_evidence(true, false, false), KnowledgeMode::Inferred);
        assert_eq!(KnowledgeMode::from_evidence(false, true, false), KnowledgeMode::NativeEnriched);
        assert_eq!(KnowledgeMode::from_evidence(false, false, true), KnowledgeMode::Contracted);
        // Contract implies native + inferred, but the evidence inputs are the
        // authority — declaring only native must not claim a contract.
        assert_eq!(KnowledgeMode::from_evidence(true, true, false), KnowledgeMode::NativeEnriched);
    }

    #[test]
    fn ordering_is_cumulative() {
        assert!(KnowledgeMode::Raw < KnowledgeMode::Inferred);
        assert!(KnowledgeMode::Inferred < KnowledgeMode::NativeEnriched);
        assert!(KnowledgeMode::NativeEnriched < KnowledgeMode::Contracted);
    }
}