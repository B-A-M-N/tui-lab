//! Finding comparison across runs (Wave G item 67): the same finding,
//! fingerprinted stably, compared across two audit passes — FIXED (was
//! present in the baseline, absent now), REGRESSED (was absent or fixed,
//! present now), NEW (not in the baseline at all — distinct from REGRESSED
//! so a first-seen defect is not mistaken for a regression caused by a
//! change).
//!
//! The fingerprint is the finding's [`Finding::occurrence_id`] (finding
//! 21): a versioned hash over SORTED key material — rule id, category,
//! every evidence target — so evidence order cannot change identity, and
//! the schema prefix bumps if what counts as "the same occurrence" ever
//! changes.

use serde_json::json;

use crate::audit::Finding;

/// A stable identity for one finding occurrence: same rule + category +
/// evidence targets (order-insensitive) = same defect in the same place.
/// Prefers the stored `occurrence_id` (assigned at `instance()`), so a
/// persisted baseline is never re-derived; computes on demand otherwise.
pub fn fingerprint(f: &Finding) -> String {
    f.occurrence_id.clone().unwrap_or_else(|| f.occurrence_id())
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

/// Fingerprints of findings a previous pass already had resolved (fixed, or
/// knowingly absent after repair). When a current finding absent from the
/// compare baseline IS in this set, it is a genuine REGRESSION (it came back)
/// rather than a first-seen NEW defect.
#[derive(Debug, Clone, Default)]
pub struct Resolved(pub std::collections::HashSet<String>);

/// Compare against a baseline, classifying regressions properly.
pub fn compare_with_resolved(
    baseline: &[Finding],
    current: &[Finding],
    resolved: &Resolved,
) -> Vec<ComparedFinding> {
    compare_impl(baseline, current, &resolved.0)
}

/// Compare the current findings against a stored baseline, classifying each
/// fresh finding as `persisting`, `new`, or (when its fingerprint was
/// previously resolved) `regressed`; baseline-only findings are `fixed`.
/// REGRESSED is genuinely reachable here — the caller supplies the set of
/// fingerprints a prior pass resolved, so a re-appearing defect is distinct
/// from a first-seen one (review P1 item 12).
///
/// - in baseline + absent now → FIXED
/// - present in both → PERSISTING
/// - absent from baseline, never resolved → NEW
/// - absent from baseline, previously resolved → REGRESSED
pub fn compare(baseline: &[Finding], current: &[Finding]) -> Vec<ComparedFinding> {
    compare_impl(baseline, current, &std::collections::HashSet::new())
}

/// Default-finding comparison: audit finding 48. Baselines compare DEFECTS
/// by default. Observations, metrics, and gates are telemetry; they must
/// not become FIXED/NEW/PERSISTING product verdicts merely because their
/// IDs or evidence changed between runs.
pub fn compare_defects(baseline: &[Finding], current: &[Finding]) -> Vec<ComparedFinding> {
    let defects = |v: &[Finding]| {
        v.iter()
            .filter(|f| f.kind == crate::audit::FindingKind::Defect)
            .cloned()
            .collect::<Vec<_>>()
    };
    compare(&defects(baseline), &defects(current))
}

fn compare_impl(
    baseline: &[Finding],
    current: &[Finding],
    resolved_keys: &std::collections::HashSet<String>,
) -> Vec<ComparedFinding> {
    let base_keys: std::collections::HashSet<String> = baseline.iter().map(fingerprint).collect();
    let cur_keys: std::collections::HashSet<String> = current.iter().map(fingerprint).collect();

    let mut out = Vec::new();
    for f in current {
        let fp = fingerprint(f);
        let verdict = if base_keys.contains(&fp) {
            "persisting"
        } else if resolved_keys.contains(&fp) {
            "regressed"
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
        // REGRESSED is a real verdict: a finding that came back after a pass
        // had resolved it (compare_with_resolved). Without a resolved set it
        // is correctly 0 — a raw snapshot baseline cannot distinguish a
        // regression from a first-seen NEW defect.
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{EvidenceKind, EvidenceRef, Severity};

    fn finding(id: &str, category: &str, target: &str) -> Finding {
        Finding {
            kind: crate::audit::FindingKind::Defect,
            id: id.into(),
            rule_id: None,
            severity: Severity::Warn,
            category: crate::audit::Category::parse(category),
            summary: format!("{} in {}", id, category),
            evidence: vec![EvidenceRef::point(EvidenceKind::Other, target, "evidence")],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    fn metric(id: &str, target: &str) -> Finding {
        Finding {
            kind: crate::audit::FindingKind::Metric,
            id: id.into(),
            rule_id: None,
            severity: Severity::Info,
            category: crate::audit::Category::Performance,
            summary: "telemetry".into(),
            evidence: vec![EvidenceRef::point(EvidenceKind::Other, target, "evidence")],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    #[test]
    fn compare_defects_excludes_telemetry_and_gate_changes() {
        let baseline = vec![
            finding("KB-TRAP", "keyboard", "tab_trap"),
            metric("PERF-OK", "observe_p95"),
        ];
        let current = vec![
            metric("PERF-OK", "observe_p95_changed"),
            finding("FOCUS-001", "focus", "focus_missing"),
        ];
        let compared = compare_defects(&baseline, &current);
        let get = |fid: &str| {
            compared
                .iter()
                .find(|c| c.finding.id == fid)
                .expect("verdict present")
        };
        assert_eq!(get("KB-TRAP").verdict, "fixed");
        assert_eq!(get("FOCUS-001").verdict, "new");
        assert!(
            !compared.iter().any(|c| c.finding.id == "PERF-OK"),
            "metric telemetry must not create baseline verdicts"
        );
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
        // Lookup by the compared finding's id (fingerprints are opaque
        // hashes now — finding 21).
        let get = |fid: &str| {
            compared
                .iter()
                .find(|c| c.finding.id == fid)
                .expect("verdict present")
        };
        assert_eq!(get("KB-TRAP").verdict, "fixed");
        assert_eq!(get("FOCUS-001").verdict, "new");
        assert_eq!(get("CLIP-001").verdict, "persisting");
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

    /// REGRESSED is genuinely reachable via the resolved set (review P1 item
    /// 12): the same fingerprint absent from the baseline but present now is
    /// `new` without a resolved marker and `regressed` with one.
    #[test]
    fn regressed_is_distinct_from_new_via_resolved_set() {
        let baseline: Vec<Finding> = Vec::new(); // an empty baseline
        let current = vec![finding("KB-TRAP", "keyboard", "tab_trap")];
        let fp = fingerprint(&current[0]);

        // No resolved marker → first-seen → NEW.
        let plain = compare(&baseline, &current);
        let v_plain = plain
            .iter()
            .find(|c| c.verdict == "new")
            .expect("new present");
        assert_eq!(v_plain.verdict, "new");

        // With the fingerprint marked resolved → genuinely REGRESSED.
        let mut resolved = Resolved::default();
        resolved.0.insert(fp);
        let with_resolved = compare_with_resolved(&baseline, &current, &resolved);
        let v_regressed = with_resolved
            .iter()
            .find(|c| c.verdict == "regressed")
            .expect("regressed present");
        assert_eq!(v_regressed.verdict, "regressed");
        let s = summary(&with_resolved);
        assert_eq!(s["regressed"], 1, "summary reflects the real verdict");
        assert_eq!(s["new"], 0);
    }
}
