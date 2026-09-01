//! Finding comparison across runs (Wave G item 67): the same finding,
//! fingerprinted stably, compared across two audit passes — FIXED (was
//! present in the baseline, absent now), REGRESSED (was absent or fixed,
//! present now), NEW (not in the baseline at all — distinct from REGRESSED
//! so a first-seen defect is not mistaken for a regression caused by a
//! change).
//!
//! The fingerprint is `category + id + stable evidence target`: finding IDs
//! like `KB-TRAP` repeat across steps, so the step-specific detail lives in
//! evidence, and the fingerprint folds in the primary evidence target (a
//! control id, region id, or evidence label) to distinguish "same defect,
//! same place" from "same class, different place".

use serde_json::json;

use crate::audit::Finding;

/// A stable identity for one finding occurrence: same category + id +
/// primary evidence target = same defect in the same place.
pub fn fingerprint(f: &Finding) -> String {
    let target = f
        .evidence
        .first()
        .and_then(|e| e.target.clone())
        .unwrap_or_default();
    format!("{}|{}|{}", f.category, f.id, target)
}

/// One compared finding: what it is, its verdict against the baseline, and
/// where each side's record lives.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComparedFinding {
    pub fingerprint: String,
    /// "fixed" | "regressed" | "new" | "persisting"
    pub verdict: &'static str,
    pub finding: Finding,
    /// The baseline finding, when one existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Finding>,
}

/// Compare the current findings against a stored baseline.
///
/// - in baseline + absent now → FIXED
/// - absent from baseline + present now → NEW
/// - present in both → PERSISTING (carried so the caller sees the full set;
///   severity changes are surfaced in the summary)
pub fn compare(baseline: &[Finding], current: &[Finding]) -> Vec<ComparedFinding> {
    let base_keys: std::collections::HashSet<String> = baseline.iter().map(fingerprint).collect();
    let cur_keys: std::collections::HashSet<String> = current.iter().map(fingerprint).collect();

    let mut out = Vec::new();
    for f in current {
        let fp = fingerprint(f);
        let verdict = if base_keys.contains(&fp) {
            "persisting"
        } else {
            "new"
        };
        let baseline_f = baseline.iter().find(|b| fingerprint(b) == fp).cloned();
        out.push(ComparedFinding {
            fingerprint: fp,
            verdict,
            finding: f.clone(),
            baseline: baseline_f,
        });
    }
    for b in baseline {
        let fp = fingerprint(b);
        if !cur_keys.contains(&fp) {
            out.push(ComparedFinding {
                fingerprint: fp,
                verdict: "fixed",
                finding: b.clone(),
                baseline: None,
            });
        }
    }
    out
}

/// JSON summary for the tool response.
pub fn summary(compared: &[ComparedFinding]) -> serde_json::Value {
    let count = |v: &str| compared.iter().filter(|c| c.verdict == v).count();
    json!({
        "fixed": count("fixed"),
        "regressed": count("regressed"),
        "new": count("new"),
        "persisting": count("persisting"),
        // Regressions in the strict sense: a finding previously recorded as
        // fixed (absent from baseline because it was never seen) is "new";
        // REGRESSED is reserved for a baseline that had marked the problem
        // resolved. The MCP layer derives regressed from labeled baselines
        // where this distinction is knowable; here it is the raw counts.
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{EvidenceKind, EvidenceRef};

    fn finding(id: &str, category: &str, target: &str) -> Finding {
        Finding {
            id: id.into(),
            rule_id: None,
            severity: "warn".into(),
            category: category.into(),
            summary: format!("{} in {}", id, category),
            evidence: vec![EvidenceRef::point(EvidenceKind::Other, target, "evidence")],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        }
    }

    #[test]
    fn fixed_new_persisting_verdicts() {
        let baseline = vec![
            finding("KB-TRAP", "keyboard", "tab_trap"),
            finding("CLIP-001", "clipping", "region-a"),
        ];
        let current = vec![
            finding("CLIP-001", "clipping", "region-a"),
            finding("FOCUS-001", "focus", "focus_missing"),
        ];
        let compared = compare(&baseline, &current);
        let get = |fp_contains: &str| {
            compared
                .iter()
                .find(|c| c.fingerprint.contains(fp_contains))
                .expect("verdict present")
        };
        assert_eq!(get("keyboard").verdict, "fixed");
        assert_eq!(get("focus").verdict, "new");
        assert_eq!(get("clipping").verdict, "persisting");
    }

    /// Same id but different evidence target = different defect, so one
    /// fixed and one persisting can coexist (per-control clipping).
    #[test]
    fn same_id_different_target_is_distinct() {
        let baseline = vec![finding("CLIP-001", "clipping", "region-a")];
        let current = vec![
            finding("CLIP-001", "clipping", "region-a"),
            finding("CLIP-001", "clipping", "region-b"),
        ];
        let compared = compare(&baseline, &current);
        let news = compared.iter().filter(|c| c.verdict == "new").count();
        let persisting = compared
            .iter()
            .filter(|c| c.verdict == "persisting")
            .count();
        assert_eq!(
            news,
            1,
            "region-b is new: {:?}",
            compared
                .iter()
                .map(|c| (c.verdict, &c.fingerprint))
                .collect::<Vec<_>>()
        );
        assert_eq!(persisting, 1, "region-a persists");
    }

    #[test]
    fn summary_counts_add_up() {
        let baseline = vec![finding("A", "cat", "t1")];
        let current = vec![finding("B", "cat", "t2")];
        let compared = compare(&baseline, &current);
        let s = summary(&compared);
        assert_eq!(s["fixed"], 1);
        assert_eq!(s["new"], 1);
        assert_eq!(s["persisting"], 0);
    }
}
