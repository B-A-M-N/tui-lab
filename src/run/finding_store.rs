//! Cohesive state for the audit-finding domain.
//!
//! Round-2 decomposition (G1): the two `RunContext` fields that describe
//! audit findings and their labeled comparison baselines move OUT of the
//! flat run bucket into this holder. `RunContext` keeps the public methods
//! as thin delegations so every caller compiles unchanged; the source-ref
//! joins that read the coverage ledger stay on the `RunContext` facade
//! (they compose across domains — findings × coverage), while pure
//! findings/baseline policy lives here.

use std::collections::HashMap;

/// Findings accumulated in this run + labeled baselines for FIXED /
/// REGRESSED / NEW comparison.
pub(super) struct FindingStore {
    /// Findings emitted during this run (audit results). Instance-unique
    /// ids are assigned by `Finding::instance()` at extend time.
    findings: Vec<crate::audit::Finding>,
    /// Wave G item 67: labeled audit-finding baselines for FIXED/REGRESSED/
    /// NEW comparison. `tui_audit label=X` stores this pass's findings under
    /// X; `tui_audit compare_to=X` diffs the fresh pass against the stored
    /// one via finding fingerprints.
    baselines: HashMap<String, Vec<crate::audit::Finding>>,
}

impl FindingStore {
    /// A fresh, empty store.
    pub(super) fn new() -> Self {
        FindingStore {
            findings: Vec::new(),
            baselines: HashMap::new(),
        }
    }

    /// Adopt already-loaded findings (restore path: the on-disk artifact is
    /// still a plain findings array; the store is the in-memory holder).
    pub(super) fn from_findings(findings: Vec<crate::audit::Finding>) -> Self {
        FindingStore {
            findings,
            baselines: HashMap::new(),
        }
    }

    /// Append findings verbatim (the caller assigns instance ids / joins
    /// source refs — cross-domain policy stays on the RunContext facade).
    pub(super) fn push_all(&mut self, findings: impl IntoIterator<Item = crate::audit::Finding>) {
        self.findings.extend(findings);
    }

    /// Findings accumulated in this run, in arrival order.
    pub(super) fn all(&self) -> &[crate::audit::Finding] {
        &self.findings
    }

    /// Evidence count for status/ledger summaries.
    pub(super) fn len(&self) -> usize {
        self.findings.len()
    }

    /// Store (or overwrite) a labeled audit-finding baseline (item 67).
    pub(super) fn record_baseline(&mut self, label: &str, findings: Vec<crate::audit::Finding>) {
        self.baselines.insert(label.to_string(), findings);
    }

    /// Fetch a labeled audit-finding baseline.
    pub(super) fn baseline(&self, label: &str) -> Option<&Vec<crate::audit::Finding>> {
        self.baselines.get(label)
    }

    /// Labels of all stored finding baselines (for honest "no such label"
    /// errors that name what exists).
    pub(super) fn baseline_labels(&self) -> Vec<String> {
        let mut l: Vec<String> = self.baselines.keys().cloned().collect();
        l.sort();
        l
    }

    /// Fingerprints of findings in ANY stored baseline OTHER than
    /// `compare_label` (review P1 item 12). A current finding absent from the
    /// compare baseline but present here was seen in an earlier pass — so its
    /// reappearance is a REGRESSION, not a first-seen NEW defect.
    pub(super) fn resolved_fingerprints(
        &self,
        compare_label: &str,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for (label, findings) in &self.baselines {
            if label == compare_label {
                continue;
            }
            for f in findings {
                out.insert(crate::audit::compare::fingerprint(f));
            }
        }
        out
    }
}
