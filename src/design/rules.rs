use super::schema::DesignContract;

/// Rules derived from a design contract.
pub struct ContractRules {
    contract: DesignContract,
}

impl ContractRules {
    /// Create a new contract rules instance.
    pub fn new(contract: DesignContract) -> Self {
        ContractRules { contract }
    }

    /// Check if a keybinding is allowed.
    pub fn is_keybinding_allowed(&self, key: &str) -> bool {
        self.contract
            .keybindings
            .iter()
            .any(|kb| kb.keys.contains(&key.to_string()))
    }

    /// Check if escape should close a modal.
    pub fn escape_closes_modal(&self) -> bool {
        self.contract.escape_closes_modal
    }

    /// Check if reverse tab is required.
    pub fn reverse_tab_required(&self) -> bool {
        self.contract.reverse_tab_required
    }

    /// Check if destructive actions require confirmation.
    pub fn destructive_requires_confirmation(&self) -> bool {
        self.contract.destructive_require_confirmation
    }

    /// Get the required viewports.
    pub fn viewports(&self) -> &[super::schema::ViewportReq] {
        &self.contract.viewports
    }

    /// Get the volatile patterns.
    pub fn volatile_patterns(&self) -> &[String] {
        &self.contract.volatile_patterns
    }
}
