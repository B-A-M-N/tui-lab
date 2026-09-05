//! Cohesive state for the design-contract domain.
//!
//! Round-2 decomposition (G1): the three `RunContext` fields that describe
//! the loaded project contract and its named conformance baselines moved
//! OUT of the flat run bucket into this holder. `RunContext` keeps the
//! public methods as thin delegations so every caller compiles unchanged;
//! this type owns the policy that "the newest contract wins" and that
//! baselines are keyed by label for `tui_contract compare`.

use std::collections::HashMap;

/// The loaded project contract plus its named conformance baselines.
///
/// `RunContext::contract()` / `set_contract()` / `contract_path()` /
/// `contract_baselines()` / `record_contract_baseline()` delegate here.
/// The status/ledger builder reads the raw values through the `pub(super)`
/// accessors below.
pub(super) struct ContractState {
    /// The loaded project contract (Wave E item 39): feeds conformance
    /// checks, exploration candidates, and audits.
    contract: Option<crate::design::ProjectContract>,
    /// Where that contract was loaded from (display/evidence).
    contract_path: Option<String>,
    /// Conformance baselines for `compare` (Wave E item 47): label → the
    /// report captured under that label.
    contract_baselines: HashMap<String, crate::design::ContractReport>,
}

impl ContractState {
    pub(super) fn new() -> Self {
        ContractState {
            contract: None,
            contract_path: None,
            contract_baselines: HashMap::new(),
        }
    }

    /// The loaded project contract, if any. Feeds exploration candidates
    /// (item 49) and `tui_contract status/compare`.
    pub(super) fn contract(&self) -> Option<&crate::design::ProjectContract> {
        self.contract.as_ref()
    }

    /// Record the loaded contract (replaces any previous one — the newest
    /// contract wins, matching the tool's load semantics). Note: the
    /// caller's manifest write is a RunContext-level persistence concern,
    /// so it happens in the delegating method, not here.
    pub(super) fn set(&mut self, contract: crate::design::ProjectContract, path: String) {
        self.contract = Some(contract);
        self.contract_path = Some(path);
    }

    /// Where the current contract was loaded from.
    pub(super) fn path(&self) -> Option<&str> {
        self.contract_path.as_deref()
    }

    /// Named conformance baselines for `tui_contract compare`: label →
    /// report. `status` writes "baseline" (the first trusted state);
    /// `compare` writes the label it was given so later comparisons have
    /// history.
    pub(super) fn baselines(&self) -> &HashMap<String, crate::design::ContractReport> {
        &self.contract_baselines
    }

    pub(super) fn record_baseline(&mut self, label: &str, report: &crate::design::ContractReport) {
        self.contract_baselines
            .insert(label.to_string(), report.clone());
    }
}
