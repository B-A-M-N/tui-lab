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
    /// to the cohesive `super::contract_state::ContractState`.
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
    /// Round-2 (G1): delegates to `super::finding_store::FindingStore`.
    pub fn record_finding_baseline(&mut self, label: &str, findings: Vec<crate::audit::Finding>) {
        self.findings.record_baseline(label, findings);
    }

    /// Beta-audit P0.10: record a completed audit pass as THE current
    /// snapshot (replaces any previous one — this is the newest pass,
    /// not history).
    pub fn record_audit_pass(&mut self, findings: Vec<crate::audit::Finding>) {
        self.findings.record_pass(findings);
    }

    /// The latest completed audit pass's findings, when one has been
    /// recorded in this run. Falls back to nothing — a run with no
    /// completed pass has no "current set" to compare against, and
    /// callers must say so rather than substitute the cumulative
    /// ledger (whose stale copies turn fixed defects into persisting
    /// ones).
    pub fn latest_audit_pass(&self) -> Option<&[crate::audit::Finding]> {
        self.findings.latest_pass()
    }

    /// Fetch a labeled audit-finding baseline.
    pub fn finding_baseline(&self, label: &str) -> Option<&Vec<crate::audit::Finding>> {
        self.findings.baseline(label)
    }

    /// Labels of all stored finding baselines (for honest "no such label"
    /// errors that name what exists).
    pub fn finding_baseline_labels(&self) -> Vec<String> {
        self.findings.baseline_labels()
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
        self.findings.resolved_fingerprints(compare_label)
    }

    /// Beta-audit P1.2: persist one `tui_workflow action=verify`
    /// execution, keyed by the finding's fingerprint, so a later caller
    /// can cite the verification instead of re-deriving it.
    pub fn record_verification(&mut self, rec: crate::audit::verification::VerificationRecord) {
        self.findings.record_verification(rec);
    }

    /// Verification records for one finding fingerprint, newest last.
    pub fn verifications_for(
        &self,
        finding_fingerprint: &str,
    ) -> Vec<crate::audit::verification::VerificationRecord> {
        self.findings.verifications_for(finding_fingerprint)
    }

    pub fn record_contract_baseline(
        &mut self,
        label: &str,
        report: &crate::design::ContractReport,
    ) {
        self.contract.record_baseline(label, report);
    }
}
