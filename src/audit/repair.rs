//! RepairPacket (audit item: "vket/RepairPacket") — the repair bundle.
//!
//! A [`Finding`] says *what* is wrong. Finding the fix then costs the agent
//! 5–8 more calls: re-derive the reproduction, find the source locus, get
//! the before/after frames, figure out how to verify the fix. The packet
//! joins everything the run already holds into one citable object:
//!
//! ```text
//! Finding ──reproduction──▶ Scenario (replayable)
//!       ───source_refs───▶ file:line loci (app-attested where coverage names them)
//!       ───evidence──────▶ frames / transactions / artifacts by id
//! ```
//!
//! Honesty rules:
//! - Every field is optional *and declared*: a packet with no source refs
//!   says `source_refs: []` — "no locus is known", never a guess.
//! - `verification` is derived from the reproduction: replaying the
//!   scenario and asserting the finding is gone. When there is no
//!   reproduction the packet says so (`verification: null`) instead of
//!   inventing one.
//! - The packet is assembled from run state; it never mutates the run.

use serde::Serialize;

/// Everything an agent needs to go from "bug detected" to "specific source
/// edit", assembled from what the run already holds.
#[derive(Debug, Clone, Serialize)]
pub struct RepairPacket {
    /// The finding this packet repairs (verbatim, with its evidence).
    pub finding: crate::audit::Finding,
    /// The replayable minimized reproduction (scenario id + steps), when
    /// the finding has one. `None` for static findings — declared, not
    /// hidden behind an empty string.
    pub reproduction: Option<ReproductionRef>,
    /// Probable cause sites in source. Empty means none are known.
    pub source_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// The loci an agent should open first (is_actionable() filter).
    pub actionable_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// How to verify a fix: replay the reproduction and expect the finding
    /// gone. `None` when no reproduction exists.
    pub verification: Option<VerificationRecipe>,
    /// Run/session correlation for citing in CI or review.
    pub run_id: String,
    pub sessions: Vec<String>,
}

/// The replayable reproduction half of a packet.
#[derive(Debug, Clone, Serialize)]
pub struct ReproductionRef {
    /// The scenario id (also `finding.reproduction`).
    pub scenario_id: String,
    /// Step count of the minimized reproduction.
    pub steps: usize,
    /// The steps themselves (canonical action JSON) — inline so the agent
    /// does not need a second call to read them.
    pub scenario: serde_json::Value,
}

/// The verification half: what to run after the edit.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationRecipe {
    /// Human/agent-readable one-liner.
    pub summary: String,
    /// The scenario to replay (same id as the reproduction).
    pub replay_scenario: String,
    /// What must NOT reappear for the fix to count.
    pub expect_absent_finding: String,
    /// The finding's rule id (the `id` field) to watch for.
    pub finding_rule_id: String,
}

impl RepairPacket {
    /// Assemble the packet for `finding` from run-held state.
    ///
    /// `load_scenario` resolves the reproduction (by id); `sessions` are
    /// the run's known session ids for correlation. Returns `None` when
    /// the finding carries no evidence (a malformed finding — callers
    /// treat that as a bug, not a packet).
    pub fn assemble(
        finding: crate::audit::Finding,
        run_id: &str,
        sessions: Vec<String>,
        load_scenario: impl Fn(&str) -> Option<crate::scenario::model::Scenario>,
    ) -> Option<Self> {
        if finding.evidence.is_empty() {
            return None;
        }
        let repro = finding.reproduction.as_deref().and_then(|id| {
            let sc = load_scenario(id)?;
            Some(ReproductionRef {
                scenario_id: id.to_string(),
                steps: sc.step_count(),
                scenario: serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            })
        });
        let verification = repro.as_ref().map(|r| VerificationRecipe {
            summary: format!(
                "Replay scenario {} ({} steps); the {} finding must not reappear.",
                r.scenario_id, r.steps, finding.id
            ),
            replay_scenario: r.scenario_id.clone(),
            expect_absent_finding: finding.summary.clone(),
            finding_rule_id: finding.id.clone(),
        });
        let actionable_refs = finding
            .source_refs
            .iter()
            .filter(|r| r.is_actionable())
            .cloned()
            .collect();
        Some(RepairPacket {
            finding,
            reproduction: repro,
            source_refs: Vec::new(), // filled by the caller from finding
            actionable_refs,
            verification,
            run_id: run_id.to_string(),
            sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{EvidenceKind, EvidenceRef};

    fn finding_with(repro: Option<&str>, refs: Vec<crate::semantic::source_ref::SourceRef>) -> crate::audit::Finding {
        crate::audit::Finding {
            id: "CLIP-001".into(),
            severity: "error".into(),
            category: "layout".into(),
            summary: "Save button clipped at right edge".into(),
            evidence: vec![EvidenceRef::point(EvidenceKind::Control, "button/save", "clipped")],
            confidence: 0.9,
            reproduction: repro.map(String::from),
            source_refs: refs,
        }
    }

    #[test]
    fn packet_joins_repro_refs_and_recipe() {
        let refs = vec![
            crate::semantic::source_ref::SourceRef {
                file: "src/ui/settings.rs".into(),
                line: 184,
                column: None,
                symbol: None,
                framework_id: None,
                confidence: 0.7,
                source: "framework-adapter".into(),
            },
            crate::semantic::source_ref::SourceRef {
                file: "guess.rs".into(),
                line: 1,
                column: None,
                symbol: None,
                framework_id: None,
                confidence: 0.2,
                source: "manual".into(),
            },
        ];
        let f = finding_with(Some("scen-42"), refs);
        let sc = crate::scenario::model::Scenario::new("repro")
            .act(serde_json::json!({"action":"key","key":"tab"}));
        let packet = RepairPacket::assemble(f, "run-1", vec!["s1".into()], |id| {
            assert_eq!(id, "scen-42");
            Some(sc.clone())
        })
        .expect("packet");
        assert_eq!(packet.reproduction.as_ref().unwrap().scenario_id, "scen-42");
        assert_eq!(packet.reproduction.as_ref().unwrap().steps, 1);
        assert!(packet.verification.is_some(), "repro → recipe");
        assert_eq!(packet.actionable_refs.len(), 1, "low-confidence guess filtered");
        assert!(packet.source_refs.is_empty(), "caller fills from finding");
    }

    #[test]
    fn static_finding_declares_no_repro_honestly() {
        let f = finding_with(None, vec![]);
        let packet =
            RepairPacket::assemble(f, "run-1", vec![], |_| None).expect("packet");
        assert!(packet.reproduction.is_none());
        assert!(packet.verification.is_none(), "no invented recipe");
        assert!(packet.actionable_refs.is_empty());
    }

    #[test]
    fn evidenceless_finding_is_rejected() {
        let mut f = finding_with(None, vec![]);
        f.evidence.clear();
        assert!(RepairPacket::assemble(f, "r", vec![], |_| None).is_none());
    }
}
