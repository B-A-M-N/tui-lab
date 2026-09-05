//! Design contracts and finding/contract baselines (label/compare support).
//!
//! Impl-family extraction (Phase 1): this child module hosts the method
//! bodies for this subsystem. Round-2 (G1): the contract-domain methods
//! now delegate to the cohesive [`super::contract_state::ContractState`];
//! the finding-baseline methods still read `RunContext::finding_baselines`
//! directly until the FindingStore extraction lands.

use super::*;

impl RunContext {
    /// The loaded project contract, if any. Feeds exploration candidates
    /// (item 49) and `tui_contract status/compare`. Round-2 (G1): delegates
    /// to the cohesive [`super::contract_state::ContractState`].
    pub fn contract(&self) -> Option<&crate::design::ProjectContract> {
        self.contract.contract()
    }

    /// Record the loaded contract (replaces any previous one — the newest
    /// contract wins, matching the tool's load semantics).
    pub fn set_contract(&mut self, contract: crate::design::ProjectContract, path: String) {
        self.contract.set(contract, path);
        self.write_manifest().ok();
    }

    /// Where the current contract was loaded from.
    pub fn contract_path(&self) -> Option<&str> {
        self.contract.path()
    }

    /// Named conformance baselines for `tui_contract compare`: label →
    /// report. `status` writes "baseline" (the first trusted state);
    /// `compare` writes the label it was given so later comparisons have
    /// history.
    pub fn contract_baselines(&self) -> &HashMap<String, crate::design::ContractReport> {
        self.contract.baselines()
    }

    /// Store (or overwrite) a labeled audit-finding baseline (item 67).
    pub fn record_finding_baseline(&mut self, label: &str, findings: Vec<crate::audit::Finding>) {
        self.finding_baselines.insert(label.to_string(), findings);
    }

    /// Fetch a labeled audit-finding baseline.
    pub fn finding_baseline(&self, label: &str) -> Option<&Vec<crate::audit::Finding>> {
        self.finding_baselines.get(label)
    }

    /// Labels of all stored finding baselines (for honest "no such label"
    /// errors that name what exists).
    pub fn finding_baseline_labels(&self) -> Vec<String> {
        let mut l: Vec<String> = self.finding_baselines.keys().cloned().collect();
        l.sort();
        l
    }

    /// Fingerprints of findings in ANY stored baseline OTHER than
    /// `compare_label` (review P1 item 12). A current finding absent from the
    /// compare baseline but present here was seen in an earlier pass — so its
    /// reappearance is a REGRESSION, not a first-seen NEW defect. This is how
    /// REGRESSED becomes genuinely reachable in `compare_with_resolved`.
    pub fn resolved_finding_fingerprints(
        &self,
        compare_label: &str,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for (label, findings) in &self.finding_baselines {
            if label == compare_label {
                continue;
            }
            for f in findings {
                out.insert(crate::audit::compare::fingerprint(f));
            }
        }
        out
    }

    pub fn record_contract_baseline(
        &mut self,
        label: &str,
        report: &crate::design::ContractReport,
    ) {
        self.contract.record_baseline(label, report);
    }
}
