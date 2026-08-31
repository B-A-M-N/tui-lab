//! Rules derived from a [`ProjectContract`] (Wave E): the query surface the
//! rest of the engine reads contracts through. Candidate generation, audits,
//! and the normalization policy all ask these instead of touching the raw
//! document.

use super::schema::ProjectContract;

pub struct ContractRules {
    contract: ProjectContract,
}

impl ContractRules {
    pub fn new(contract: ProjectContract) -> Self {
        ContractRules { contract }
    }

    /// The underlying document.
    pub fn contract(&self) -> &ProjectContract {
        &self.contract
    }

    /// Check if a key is declared for any action in the contract.
    pub fn is_keybinding_allowed(&self, key: &str) -> bool {
        self.contract
            .keybindings
            .iter()
            .any(|kb| kb.keys.iter().any(|k| k == key))
            || self
                .contract
                .interactions
                .iter()
                .any(|i| i.keys.iter().any(|k| k == key))
    }

    /// Property: Escape should close a modal.
    pub fn escape_closes_modal(&self) -> bool {
        self.contract.escape_closes_modal
    }

    /// Property: Shift+Tab must exactly reverse Tab.
    pub fn reverse_tab_required(&self) -> bool {
        self.contract.reverse_tab_required
    }

    /// Property: destructive actions require confirmation.
    pub fn destructive_requires_confirmation(&self) -> bool {
        self.contract.destructive_require_confirmation
    }

    /// The required viewports.
    pub fn viewports(&self) -> &[super::schema::ViewportReq] {
        &self.contract.viewports
    }

    /// Declared keys: every key in the legacy keybinding table plus every
    /// interaction's key sequence, deduplicated, declaration order kept.
    /// Candidate generation treats these as coverage claims.
    pub fn declared_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for kb in &self.contract.keybindings {
            for k in &kb.keys {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
        }
        for inter in &self.contract.interactions {
            for k in &inter.keys {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::schema::{
        ComponentContract, InteractionContract, Keybinding, ProjectContract,
    };

    fn sample() -> ProjectContract {
        ProjectContract {
            keybindings: vec![Keybinding {
                action: "quit".into(),
                keys: vec!["q".into()],
            }],
            interactions: vec![InteractionContract {
                name: "open-add-dialog".into(),
                keys: vec!["n".into()],
                context: None,
                expect: vec!["modal_open()".into()],
            }],
            components: vec![ComponentContract {
                name: "main-table".into(),
                role: "table".into(),
                required: true,
                expect: vec![],
            }],
            ..ProjectContract::default()
        }
    }

    #[test]
    fn declared_keys_merges_both_sources() {
        let rules = ContractRules::new(sample());
        let keys = rules.declared_keys();
        assert!(keys.contains(&"q".to_string()));
        assert!(keys.contains(&"n".to_string()));
        assert_eq!(keys.len(), 2, "deduplicated: {:?}", keys);
    }

    #[test]
    fn keybinding_allowed_reads_both_tables() {
        let rules = ContractRules::new(sample());
        assert!(rules.is_keybinding_allowed("q"));
        assert!(rules.is_keybinding_allowed("n"));
        assert!(!rules.is_keybinding_allowed("x"));
    }
}
