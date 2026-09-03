//! DiagnosticContext (review §2/§3; formerly `RepairPacket`).
//!
//! A [`Finding`] says *what* is wrong. Investigating it then costs the
//! agent 5–8 more calls: re-derive the reproduction, find the source
//! loci, get the before/after frames, figure out how to verify a change.
//! The context joins everything the run already holds into one citable
//! object:
//!
//! ```text
//! Finding ──reproduction──▶ Scenario (replayable, when one exists)
//!       ───source_refs───▶ file:line loci (tiered by provenance)
//!       ───evidence──────▶ frames / transactions / artifacts by id
//!       ───verification──▶ targeted checks (NOT edit instructions)
//! ```
//!
//! Honesty rules (review §2: this is an instrument for investigation,
//! not a fix-oriented contract):
//! - Every field is optional *and declared*: a context with no source
//!   loci says `source_refs: []` — "no locus is known", never a guess.
//! - `verification` is a PLAN, decoupled from reproduction (review §3):
//!   a static finding with excellent evidence gets targeted checks
//!   without anyone inventing a scenario. Only the replay half is
//!   `None`-gated on an actual reproduction.
//! - `suggested_next_observations` are observation-shaped ("probe this
//!   control after Tab", "capture the redraw frames") — never
//!   edit-shaped ("change width", "apply this patch"). The coding agent
//!   decides the fix; this surface only increases knowledge.
//! - The context is assembled from run state; it never mutates the run.

use serde::Serialize;

/// Everything an agent needs to go from "bug detected" to an informed
/// investigation — assembled from what the run already holds. (The
/// former name, `RepairPacket`, promised an edit this surface never
/// makes; the fields were already evidence-shaped, so the contract now
/// says what it does.)
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticContext {
    /// The finding under investigation (verbatim, with its evidence).
    pub finding: crate::audit::Finding,
    /// Wave 5 item 42: the RULE identity (`rule_id`, falling back to the
    /// instance id when a producer never set the rule) — the stable key
    /// across runs. Verification and regression bundles key on this, not
    /// on the instance id, which is unique per occurrence.
    pub rule_id: String,
    /// The replayable minimized reproduction (scenario id + steps), when
    /// the finding has one. `None` for static findings — declared, not
    /// hidden behind an empty string.
    pub reproduction: Option<ReproductionRef>,
    /// Known source loci, best first, each carrying its own
    /// [`crate::semantic::source_ref::Provenance`]. Empty means none are
    /// known — never a guess.
    pub source_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// The loci an agent may open first: the attested, above-the-fence
    /// subset ([`SourceRef::is_actionable()`]). A correlated or inferred
    /// locus stays in `source_refs` as investigative evidence.
    pub actionable_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// How a future change can be verified (review §3: decoupled from
    /// reproduction — targeted checks exist for any finding with
    /// evidence; only the replay half needs an actual scenario).
    pub verification: VerificationPlan,
    /// What to look at NEXT (review §2): observation-shaped suggestions
    /// derived from the finding's own evidence — a probe, a mode
    /// timeline, a resize compare, a source read. Never an edit.
    pub suggested_next_observations: Vec<NextObservation>,
    /// Run/session correlation for citing in CI or review.
    pub run_id: String,
    pub sessions: Vec<String>,
}

/// The replayable reproduction half of a context.
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

/// The verification half (review §3): a plan exists for any finding with
/// evidence; the replay is a separate, optional leg that needs an actual
/// reproduction scenario.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationPlan {
    /// Human/agent-readable one-liner over the whole plan.
    pub summary: String,
    /// Targeted re-checks derived from the finding's own evidence — the
    /// narrowest things an agent can run to confirm a change addressed
    /// the finding. Present for any finding with a target.
    pub targeted_checks: Vec<TargetedCheck>,
    /// The replay leg: replay this scenario and expect the finding gone.
    /// `None` when no reproduction exists — the targeted checks above
    /// still stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayCheck>,
}

/// Wave 5 item 44: one targeted re-check derived from the finding's own
/// evidence — the narrowest thing an agent can run to confirm a change.
#[derive(Debug, Clone, Serialize)]
pub struct TargetedCheck {
    /// The evidence target the finding anchored on (control id, region,
    /// frame hash …).
    pub target: String,
    /// The cheapest audit surface that re-observes this target.
    pub recheck_hint: String,
}

/// The replay leg of a verification plan.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayCheck {
    /// The scenario to replay.
    pub replay_scenario: String,
    /// What must NOT reappear for the change to count.
    pub expect_absent_finding: String,
    /// The RULE id to watch for after the change (Wave 5 item 42):
    /// stable across runs, so a fresh audit pass comparing rule keys
    /// reports FIXED/REGRESSED correctly even though instance ids differ.
    pub finding_rule_id: String,
}

/// Review §2: one observation-shaped next step. An instruction to LOOK,
/// not to change: probe, inspect, compare, replay, read, capture.
#[derive(Debug, Clone, Serialize)]
pub struct NextObservation {
    /// Short imperative in observation space, e.g.
    /// "probe control 'button/save' after Tab".
    pub suggestion: String,
    /// Why this observation would move the investigation.
    pub rationale: String,
}

impl DiagnosticContext {
    /// Assemble the context for `finding` from run-held state.
    ///
    /// `load_scenario` resolves the reproduction (by id); `sessions` are
    /// the run's known session ids for correlation. Returns `None` when
    /// the finding carries no evidence (a malformed finding — callers
    /// treat that as a bug, not a context).
    pub fn assemble(
        finding: crate::audit::Finding,
        run_id: &str,
        sessions: Vec<String>,
        load_scenario: impl Fn(&str) -> Option<crate::scenario::model::Scenario>,
    ) -> Option<Self> {
        if finding.evidence.is_empty() {
            return None;
        }
        // Wave 5 item 42: rule identity is the stable verification key.
        let rule_id = finding
            .rule_id
            .clone()
            .unwrap_or_else(|| finding.id.clone());
        // Targeted checks come from the finding's own evidence — one per
        // distinct target, so a multi-target finding verifies narrowly on
        // every locus it named (review §3: not gated on a scenario).
        let mut targeted_checks: Vec<TargetedCheck> = Vec::new();
        let mut targets_seen: Vec<&str> = Vec::new();
        for e in &finding.evidence {
            let Some(t) = e.target.as_deref() else {
                continue;
            };
            if targets_seen.contains(&t) {
                continue;
            }
            targets_seen.push(t);
            targeted_checks.push(TargetedCheck {
                target: t.to_string(),
                recheck_hint: format!(
                    "re-run the '{}' audit (or tui_probe against this target) and confirm the '{}' rule no longer fires here",
                    finding.category, rule_id
                ),
            });
        }
        let repro = finding.reproduction.as_deref().and_then(|id| {
            let sc = load_scenario(id)?;
            Some(ReproductionRef {
                scenario_id: id.to_string(),
                steps: sc.step_count(),
                scenario: serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            })
        });
        // ReplayCheck is the only leg that needs a real reproduction.
        let replay = repro.as_ref().map(|r| ReplayCheck {
            replay_scenario: r.scenario_id.clone(),
            expect_absent_finding: finding.summary.clone(),
            finding_rule_id: rule_id.clone(),
        });
        let target_list = targeted_checks
            .first()
            .map(|c| c.target.clone())
            .unwrap_or_else(|| "its evidence target".to_string());
        let summary = match (&repro, targeted_checks.is_empty()) {
            (Some(r), false) => format!(
                "Re-check target '{}' (re-run the '{}' audit or tui_probe); then replay scenario {} ({} steps) — the rule {} must not reappear.",
                target_list, finding.category, r.scenario_id, r.steps, rule_id
            ),
            (Some(r), true) => format!(
                "Replay scenario {} ({} steps); the rule {} must not reappear.",
                r.scenario_id, r.steps, rule_id
            ),
            (None, false) => format!(
                "Re-check target '{}' (re-run the '{}' audit or tui_probe) and confirm the '{}' rule no longer fires.",
                target_list, finding.category, rule_id
            ),
            (None, true) => format!(
                "Re-run the '{}' audit and confirm the '{}' rule no longer fires.",
                finding.category, rule_id
            ),
        };
        let actionable_refs = finding
            .source_refs
            .iter()
            .filter(|r| r.is_actionable())
            .cloned()
            .collect();
        let suggested_next_observations = next_observations(&finding);
        Some(DiagnosticContext {
            rule_id,
            finding,
            reproduction: repro,
            source_refs: Vec::new(), // filled by the caller from finding
            actionable_refs,
            verification: VerificationPlan {
                summary,
                targeted_checks,
                replay,
            },
            suggested_next_observations,
            run_id: run_id.to_string(),
            sessions,
        })
    }
}

/// Review §2: observation-shaped next steps derived from the finding's
/// own evidence — what to LOOK at to sharpen the diagnosis, never an
/// edit instruction.
fn next_observations(finding: &crate::audit::Finding) -> Vec<NextObservation> {
    let mut out = Vec::new();
    for e in finding.evidence.iter() {
        let Some(target) = e.target.as_deref() else {
            continue;
        };
        match e.kind {
            crate::audit::EvidenceKind::Control => {
                out.push(NextObservation {
                    suggestion: format!("tui_probe the control '{target}' after focusing it — capture the settled frame and its material changes"),
                    rationale: "a control-targeted probe separates 'the control is absent' from 'the control is present but clipped/unresponsive'".into(),
                });
                out.push(NextObservation {
                    suggestion: "read the source loci in source_refs (opening with the attested ones) before editing anything".to_string(),
                    rationale: "provenance-tiered loci point at where the evidence was earned; correlated loci are leads, not cause sites".into(),
                });
            }
            crate::audit::EvidenceKind::Region => {
                out.push(NextObservation {
                    suggestion: format!("capture frames at two terminal sizes (tui_act resize, then tui_observe) and compare region '{target}'"),
                    rationale: "a region complaint that moves with size is layout math; one that persists across sizes is content or styling".into(),
                });
            }
            _ => {
                out.push(NextObservation {
                    suggestion: format!("inspect the run's transaction ledger around the finding's evidence target '{target}'"),
                    rationale: "the interaction that preceded the observation often names the trigger the finding only implies".into(),
                });
            }
        }
    }
    // Deduplicate by suggestion (multi-evidence findings repeat kinds).
    out.dedup_by(|a, b| a.suggestion == b.suggestion);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{EvidenceKind, EvidenceRef};

    fn finding_with(
        repro: Option<&str>,
        refs: Vec<crate::semantic::source_ref::SourceRef>,
    ) -> crate::audit::Finding {
        crate::audit::Finding {
            id: "CLIP-001".into(),
            rule_id: None,
            severity: "error".into(),
            category: "layout".into(),
            summary: "Save button clipped at right edge".into(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Control,
                "button/save",
                "clipped",
            )],
            confidence: 0.9,
            reproduction: repro.map(String::from),
            source_refs: refs,
        }
    }

    #[test]
    fn context_joins_repro_refs_and_plan() {
        let refs = vec![
            crate::semantic::source_ref::SourceRef {
                file: "src/ui/settings.rs".into(),
                line: 184,
                column: None,
                symbol: None,
                framework_id: None,
                confidence: 0.7,
                source: "framework-adapter".into(),
                provenance: crate::semantic::source_ref::Provenance::Attested,
            },
            crate::semantic::source_ref::SourceRef {
                file: "guess.rs".into(),
                line: 1,
                column: None,
                symbol: None,
                framework_id: None,
                confidence: 0.2,
                source: "manual".into(),
                provenance: crate::semantic::source_ref::Provenance::Inferred,
            },
        ];
        let f = finding_with(Some("scen-42"), refs);
        let sc = crate::scenario::model::Scenario::new("repro")
            .act(serde_json::json!({"action":"key","key":"tab"}));
        let ctx = DiagnosticContext::assemble(f, "run-1", vec!["s1".into()], |id| {
            assert_eq!(id, "scen-42");
            Some(sc.clone())
        })
        .expect("context");
        assert_eq!(ctx.reproduction.as_ref().unwrap().scenario_id, "scen-42");
        assert_eq!(ctx.reproduction.as_ref().unwrap().steps, 1);
        let plan = &ctx.verification;
        assert!(plan.replay.is_some(), "repro → replay leg");
        assert_eq!(plan.targeted_checks.len(), 1);
        assert_eq!(plan.targeted_checks[0].target, "button/save");
        assert_eq!(
            ctx.actionable_refs.len(),
            1,
            "inferred guess stays in source_refs, out of actionable"
        );
        assert!(ctx.source_refs.is_empty(), "caller fills from finding");
        // Review §2: suggestions observe, they do not edit.
        for obs in &ctx.suggested_next_observations {
            let s = obs.suggestion.to_ascii_lowercase();
            assert!(
                !s.contains("apply ") && !s.contains("replace ") && !s.contains("patch"),
                "suggestions must be observation-shaped: {}",
                obs.suggestion
            );
        }
    }

    /// Review §3's exact defect: a static finding (no reproduction) with
    /// excellent evidence used to get `verification: null`. The plan now
    /// carries targeted checks regardless; only the replay leg is gated.
    #[test]
    fn static_finding_gets_a_verification_plan_without_inventing_a_scenario() {
        let f = finding_with(None, vec![]);
        let ctx = DiagnosticContext::assemble(f, "run-1", vec![], |_| None).expect("context");
        assert!(ctx.reproduction.is_none(), "declared, not hidden");
        let plan = &ctx.verification;
        assert!(
            !plan.targeted_checks.is_empty(),
            "targeted checks exist without a scenario"
        );
        assert!(plan.replay.is_none(), "no invented replay");
        assert!(plan.targeted_checks[0].recheck_hint.contains("CLIP-001"));
        assert!(ctx.actionable_refs.is_empty());
    }

    #[test]
    fn evidenceless_finding_is_rejected() {
        let mut f = finding_with(None, vec![]);
        f.evidence.clear();
        assert!(DiagnosticContext::assemble(f, "r", vec![], |_| None).is_none());
    }
}
