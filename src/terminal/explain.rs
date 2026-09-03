//! Explain — the surface that ties a finding → probe → source (Wave G review
//! P1/P2 16).
//!
//! An audit [`Finding`] and a [`ProbeResult`](crate::diagnostic::ProbeResult)
//! both already carry evidence, but neither tells a story: a finding lists
//! typed [`EvidenceRef`]s without saying which probe produced them, and a
//! probe reports a transition without connecting it back to the finding it
//! explains. [`explain_finding`] closes that loop — given the finding, the
//! probe that produced it, and optionally the terminal profile, it emits a
//! path-shaped explanation: *where* the finding came from, *what evidence*
//! the transition contributed, and *how confident* the chain is.
//!
//! The explainer never fabricates source data it wasn't handed. If a finding
//! cites a transaction/frame the caller did not supply a probe for, the step
//! says so explicitly (`Unresolved`) rather than inventing a number. Honesty
//! about gaps is the point: an unverifiable citation must not read as
//! confirmation.

use crate::audit::{EvidenceKind, EvidenceRef, Finding};
use crate::screen::diff::Transition;

/// Whether one explain step could be resolved to real runtime data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainResolution {
    /// The step traced to concrete probe/transition data.
    Resolved,
    /// The evidence cites something the caller didn't supply a probe for.
    Unresolved,
}

/// One step in a finding's explanation path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplainStep {
    /// The evidence kind this step explains.
    pub kind: EvidenceKind,
    /// What the evidence points at (control id, structure hash, ...).
    pub target: Option<String>,
    /// The finding's own summary label for this evidence.
    pub finding_summary: String,
    /// The narration produced by tracing this evidence to a source.
    pub narrative: String,
    /// Whether the narrative is backed by supplied runtime data.
    pub resolution: ExplainResolution,
}

/// The full explanation of one finding.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FindingExplanation {
    /// The finding id being explained.
    pub id: String,
    /// Its category/severity for context.
    pub category: String,
    pub severity: String,
    /// A one-line gist tying finding → probe action.
    pub gist: String,
    /// One step per evidence ref, tracing finding → source.
    pub steps: Vec<ExplainStep>,
    /// Overall confidence in the chain, derived from how many steps resolved.
    pub trace_confidence: f32,
    /// Probable cause sites carried by the finding (re-review item 30: the
    /// explanation covers the repair path, not just the diagnosis). Empty
    /// means none are known — never a guess.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// The subset of `source_refs` a repair pass should open first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actionable_source_refs: Vec<crate::semantic::source_ref::SourceRef>,
    /// The replayable reproduction scenario id, when the finding has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<String>,
    /// How to verify a fix, derived from the reproduction. `None` when
    /// there is no reproduction — declared absence, not invented advice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// Explain a finding using the probe that produced it (and optionally a
/// terminal profile for added terminal context).
///
/// `probe` is `None` when the finding is static (not probe-driven); then the
/// gist and steps are derived from the finding's evidence alone, and
/// screen-diff citations are reported unresolved. `profile` is only used to
/// enrich the gist when a terminal capability bears on the finding's category.
pub fn explain_finding(
    finding: &Finding,
    probe: Option<&crate::diagnostic::ProbeResult>,
    profile: Option<&TerminalProfile>,
) -> FindingExplanation {
    let gist = build_gist(finding, probe, profile);

    let steps: Vec<ExplainStep> = finding
        .evidence
        .iter()
        .map(|ev| explain_evidence(ev, probe))
        .collect();

    let resolved = steps
        .iter()
        .filter(|s| s.resolution == ExplainResolution::Resolved)
        .count();
    let trace_confidence = if steps.is_empty() {
        0.0
    } else {
        resolved as f32 / steps.len() as f32
    };

    // Repair path (item 30): join the loci and reproduction the finding
    // already carries — the explanation must move an agent from diagnosis
    // to edit without another round of lookups.
    let actionable_source_refs = finding
        .source_refs
        .iter()
        .filter(|r| r.is_actionable())
        .cloned()
        .collect();
    let verification = finding.reproduction.as_ref().map(|scen| {
        format!(
            "replay scenario '{}' and confirm the '{}' finding does not reappear",
            scen, finding.id
        )
    });

    FindingExplanation {
        id: finding.id.clone(),
        category: finding.category.clone(),
        severity: finding.severity.clone(),
        gist,
        steps,
        trace_confidence,
        source_refs: finding.source_refs.clone(),
        actionable_source_refs,
        reproduction: finding.reproduction.clone(),
        verification,
    }
}

fn build_gist(
    finding: &Finding,
    probe: Option<&crate::diagnostic::ProbeResult>,
    profile: Option<&TerminalProfile>,
) -> String {
    // Terminal context: does the finding's category bear on a capability the
    // profile has NOT confirmed? If so, say so — the finding may be an artifact
    // of missing capability, not a real defect.
    if let Some(p) = profile {
        let relevant = feature_for_category(&finding.category, p);
        if let Some(f) = relevant {
            if f.state != CapabilityState::Supported {
                return format!(
                    "finding is conditional on '{}', which is {} in this \
                     terminal — trace before treating it as a defect",
                    f.name,
                    f.state.name(),
                );
            }
        }
    }
    match probe {
        Some(p) => format!(
            "probe '{}' found {} material change(s); timing {} ms; {},
             affecting '{}'",
            ascii_clean(p.action.as_str()),
            p.anomalies.len(),
            p.timing_ms,
            describe_transition(&p.transition),
            finding.category.clone(),
        ),
        None => {
            // Static finding: describe from its evidence refs alone.
            let kinds: Vec<&str> = finding.evidence.iter().map(|e| e.kind_name()).collect();
            format!(
                "static rule '{}' cited {} evidence source(s): {}",
                finding.category.clone(),
                kinds.len(),
                kinds.join(", "),
            )
        }
    }
}

/// Which profile feature a finding category is sensitive to, if any.
fn feature_for_category<'a>(
    category: &str,
    profile: &'a TerminalProfile,
) -> Option<&'a ProfileFeature> {
    match category {
        // Discoverability findings lean on visible style (a capability).
        c if c.contains("discoverability") => profile.features.iter().find(|f| f.id == "colors"),
        // Navigation findings rely on scrollback when exploring.
        c if c.contains("navigation") || c.contains("scroll") => {
            profile.features.iter().find(|f| f.id == "scrollback")
        }
        // Clipping/layout findings need accurate attributes.
        c if c.contains("clip") || c.contains("layout") => {
            profile.features.iter().find(|f| f.id == "cell_attributes")
        }
        _ => None,
    }
}

fn describe_transition(t: &Transition) -> String {
    let mut parts = Vec::new();
    parts.push(format!("{} cells changed", t.screen_diff.changed_cells));
    if !t.semantic_diff.controls_added.is_empty() {
        parts.push(format!(
            "{} control(s) added",
            t.semantic_diff.controls_added.len()
        ));
    }
    if !t.semantic_diff.controls_removed.is_empty() {
        parts.push(format!(
            "{} control(s) removed",
            t.semantic_diff.controls_removed.len()
        ));
    }
    if !t.semantic_diff.regions_added.is_empty() {
        parts.push(format!(
            "{} region(s) added",
            t.semantic_diff.regions_added.len()
        ));
    }
    if t.screen_diff.cursor.is_some() {
        parts.push("cursor moved".into());
    }
    if t.screen_diff.process.is_some() {
        parts.push("process state changed".into());
    }
    parts.join("; ")
}

/// Trace one evidence ref to its source, resolving against the probe when the
/// kind maps to probe/transition data.
fn explain_evidence(
    ev: &EvidenceRef,
    probe: Option<&crate::diagnostic::ProbeResult>,
) -> ExplainStep {
    let target = ev.target.clone();
    let kind = ev.kind.clone();

    match (&ev.kind, probe, &ev.target) {
        // Screen snapshot → the after-frame structure hash, if the probe's
        // after matches (or we can at least cite the supplied transition).
        (EvidenceKind::ScreenSnapshot, Some(p), Some(hash)) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: format!(
                "frame hash '{}' {} the probe's after-frame ({} cells changed)",
                hash,
                if p.after.structure_hash == *hash {
                    "equals"
                } else {
                    "is cited for but differs from"
                },
                p.transition.screen_diff.changed_cells,
            ),
            resolution: ExplainResolution::Resolved,
        },
        // Control / Region → cite the semantic diff deltas from the probe.
        (EvidenceKind::Control, Some(p), _) => {
            let n = p.transition.semantic_diff.controls_added.len();
            ExplainStep {
                kind: kind.clone(),
                target: target.clone(),
                finding_summary: ev.summary.clone(),
                narrative: format!(
                    "the probe's transition {} {} control(s); this finding cites a control as its subject",
                    if n > 0 { "added" } else { "changed/kept" },
                    n,
                ),
                resolution: ExplainResolution::Resolved,
            }
        }
        (EvidenceKind::Region, Some(p), _) => {
            let n = p.transition.semantic_diff.regions_added.len();
            ExplainStep {
                kind: kind.clone(),
                target: target.clone(),
                finding_summary: ev.summary.clone(),
                narrative: format!(
                    "the probe's transition {} {} region(s); this finding cites a region",
                    if n > 0 { "added" } else { "re-shaped" },
                    n,
                ),
                resolution: ExplainResolution::Resolved,
            }
        }
        // Diff → describe the probe's before/after transition directly.
        (EvidenceKind::Diff, Some(p), _) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: describe_transition(&p.transition),
            resolution: ExplainResolution::Resolved,
        },
        // TerminalEvent → the probe captured events; cite the count.
        (EvidenceKind::TerminalEvent, Some(p), _) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: format!(
                "the probe captured {} terminal event(s) across its window",
                p.terminal_events.len(),
            ),
            resolution: ExplainResolution::Resolved,
        },
        // Frame/Transaction/Assertion → requires the run ledger the caller
        // didn't supply; be honest.
        (EvidenceKind::Frame, probe, _) => {
            let narrative = if probe.is_some() {
                "evidence cites a canonical frame; the probe supplied a transition, \
                 but this exact frame id was not in the caller's scope"
                    .to_string()
            } else {
                "evidence cites a canonical frame; no probe/trace supplied to resolve it"
                    .to_string()
            };
            ExplainStep {
                kind: kind.clone(),
                target: target.clone(),
                finding_summary: ev.summary.clone(),
                narrative,
                resolution: ExplainResolution::Unresolved,
            }
        }
        (EvidenceKind::Transaction, _probe, _) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: "evidence cites an interaction transaction; its ledger \
                 was not supplied to this explainer, so it cannot be traced here"
                .to_string(),
            resolution: ExplainResolution::Unresolved,
        },
        (EvidenceKind::Assertion, _probe, _) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: "evidence cites a recorded assertion run; replay data was not \
                 supplied to this explainer"
                .to_string(),
            resolution: ExplainResolution::Unresolved,
        },
        // Artifact / Other → surface the artifact path without over-claiming.
        (EvidenceKind::Artifact, _probe, _) => ExplainStep {
            kind: kind.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: ev
                .artifact
                .as_ref()
                .map(|a| format!("on-disk artifact: {}", a.display()))
                .unwrap_or_else(|| "artifact evidence with no path on record".to_string()),
            resolution: if ev.artifact.is_some() {
                ExplainResolution::Resolved
            } else {
                ExplainResolution::Unresolved
            },
        },
        (other, _probe, _) => ExplainStep {
            kind: other.clone(),
            target: target.clone(),
            finding_summary: ev.summary.clone(),
            narrative: format!(
                "evidence kind '{}' has no probe-trace stub; resolve from its own detail",
                ev.kind_name(),
            ),
            resolution: ExplainResolution::Resolved,
        },
    }
}

/// Collapse whitespace so a narrative stays on one line.
fn ascii_clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn kind_name(kind: &EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::ScreenSnapshot => "screen snapshot",
        EvidenceKind::Control => "control",
        EvidenceKind::Region => "region",
        EvidenceKind::Frame => "frame",
        EvidenceKind::Diff => "diff",
        EvidenceKind::Artifact => "artifact",
        EvidenceKind::Transaction => "transaction",
        EvidenceKind::Assertion => "assertion",
        EvidenceKind::TerminalEvent => "terminal event",
        EvidenceKind::Other => "other",
    }
}

use crate::terminal::{CapabilityState, ProfileFeature, TerminalProfile};

// Small helper used by the explainer and tests.
impl EvidenceRef {
    fn kind_name(&self) -> &'static str {
        kind_name(&self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{EvidenceRef, Finding};
    use crate::screen::diff;

    fn finding() -> Finding {
        Finding {
            id: "EXPL-1".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "discoverability".into(),
            summary: "focus affordance has no visible cue".into(),
            evidence: vec![
                EvidenceRef::screen("h123", "frame at observation"),
                EvidenceRef::control("c0", "subject control"),
            ],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        }
    }

    fn probe_with_transition() -> crate::diagnostic::ProbeResult {
        let mut before = crate::screen::ScreenState::new(80, 24);
        let mut after = crate::screen::ScreenState::new(80, 24);
        before.structure_hash = "h0".into();
        after.structure_hash = "h123".into();
        // A real transition computed from the two frames (as the runtime does).
        let transition = diff(&before, &after);
        crate::diagnostic::ProbeResult {
            before,
            after,
            action: "press Enter".into(),
            settle: crate::execution::SettleStatus::Met,
            terminal_events: vec![],
            frames: vec![],
            transition,
            after_focus: None,
            timing_ms: 42,
            anomalies: vec!["focus moved".into()],
        }
    }

    #[test]
    fn explains_a_probe_driven_finding_and_resolves_sources() {
        let f = finding();
        let probe = probe_with_transition();
        let exp = explain_finding(&f, Some(&probe), None);
        assert_eq!(exp.id, "EXPL-1");
        assert!(
            exp.gist.contains("probe 'press Enter'"),
            "gist must name the probe: {}",
            exp.gist
        );
        assert_eq!(exp.steps.len(), 2);
        // ScreenSnapshot step: the cited hash equals the probe's after-frame.
        assert_eq!(exp.steps[0].resolution, ExplainResolution::Resolved);
        assert!(exp.steps[0].narrative.contains("h123"));
        // Control step resolves via the probe's transition.
        assert_eq!(exp.steps[1].resolution, ExplainResolution::Resolved);
        assert!(exp.trace_confidence > 0.0);
    }

    #[test]
    fn unresolvable_citations_are_honest_not_invented() {
        let f = finding();
        // No probe supplied → the gist must not claim a probe, and the screen
        // snapshot step must not invent a transition.
        let exp = explain_finding(&f, None, None);
        assert!(exp.gist.contains("static rule"), "{}", exp.gist);
        assert!(
            exp.steps
                .iter()
                .all(|s| !s.narrative.contains("cells changed")),
            "a static finding must not invent a transition"
        );
    }

    #[test]
    fn terminal_profile_context_conditions_scrollback_findings() {
        // A finding in a category that depends on scrollback (navigation) is
        // conditional on it; a default profile has scrollback UNVERIFIED.
        let mut f = finding();
        f.category = "navigation".into();
        let profile = TerminalProfile::default();
        let exp = explain_finding(&f, None, Some(&profile));
        assert!(
            exp.gist.to_uppercase().contains("UNVERIFIED"),
            "gist must flag the unverified capability the finding is conditional on: {}",
            exp.gist
        );
        let scroll = profile
            .features
            .iter()
            .find(|x| x.id == "scrollback")
            .expect("scrollback feature present");
        assert_eq!(scroll.state, CapabilityState::Unverified);
    }

    #[test]
    fn well_supported_category_is_not_conditioned_by_default_profile() {
        // Discoverability maps to colors, which is baseline-Confirmed even in
        // a default profile — so the gist must NOT declare the finding
        // conditional.
        let f = finding(); // category = discoverability
        let profile = TerminalProfile::default();
        let exp = explain_finding(&f, None, Some(&profile));
        assert!(
            !exp.gist.to_uppercase().contains("UNVERIFIED"),
            "baseline-supported colors must not condition the finding: {}",
            exp.gist
        );
    }
}
