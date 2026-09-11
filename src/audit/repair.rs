//! DiagnosticContext (review §2/§3; formerly `RepairPacket`).
//!
//! A `Finding` says *what* is wrong. Investigating it then costs the
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
    /// subset (`SourceRef::is_actionable()`). A correlated or inferred
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
///
/// Finding 23: the step carries its invocation STRUCTURALLY — the MCP tool
/// name and the exact arguments — so an agent can act without parsing the
/// prose. `suggestion`/`rationale` remain for display; `tool` +
/// `arguments` are the contract.
///
/// Beta-audit P0.4: `arguments` are built FROM the real MCP parameter
/// types (`ToolInvocation`), never hand-written JSON. Every generated
/// step round-trips through its tool's `Deserialize` before it is
/// emitted — a suggestion that does not deserialize is a build/test
/// failure, not an agent-side surprise.
#[derive(Debug, Clone, Serialize)]
pub struct NextObservation {
    /// Short imperative in observation space, e.g.
    /// "inspect control 'button/save' with tui_observe mode=inspect".
    pub suggestion: String,
    /// Why this observation would move the investigation.
    pub rationale: String,
    /// The MCP tool that performs this observation (`tui_probe`,
    /// `tui_observe`, …). `None` when the step is a human-space read
    /// (source files) with no tool surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The exact arguments for `tool`, ready to send. Empty object when
    /// `tool` is `None` or the step needs no parameters.
    pub arguments: serde_json::Value,
}

/// Beta-audit P0.4: the typed source of every generated invocation.
/// Building from the REAL parameter types (then serializing) is the
/// only way the "exact arguments, ready to send" promise can be kept —
/// hand-built JSON has already drifted into an invalid grammar once
/// (nested `action.kind` that no tool accepts, fabricated session ids,
/// a `tui_run context` claimed to be the transaction ledger).
enum ToolInvocation {
    Observe(crate::mcp::params::TuiObserveParams),
    Probe(crate::mcp::params::TuiProbeParams),
    Act(crate::mcp::params::TuiActRequest),
}

impl ToolInvocation {
    /// The wire shape: serialize the REAL type, then round-trip it
    /// back through that type's `Deserialize` to prove the generated
    /// arguments are actually accepted. Returns `None` if the
    /// round-trip fails — which is a generator bug and must surface
    /// in tests, never in an agent's session.
    fn to_arguments(&self) -> Option<serde_json::Value> {
        let (tool, value): (&str, serde_json::Value) = match self {
            ToolInvocation::Observe(p) => ("tui_observe", serde_json::to_value(p).ok()?),
            ToolInvocation::Probe(p) => ("tui_probe", serde_json::to_value(p).ok()?),
            ToolInvocation::Act(p) => ("tui_act", serde_json::to_value(p).ok()?),
        };
        // Round-trip validation: the arguments must deserialize into
        // the tool's real request type.
        let ok = match tool {
            "tui_observe" => {
                serde_json::from_value::<crate::mcp::params::TuiObserveParams>(value.clone())
                    .is_ok()
            }
            "tui_probe" => {
                serde_json::from_value::<crate::mcp::params::TuiProbeParams>(value.clone()).is_ok()
            }
            "tui_act" => {
                serde_json::from_value::<crate::mcp::params::TuiActRequest>(value.clone()).is_ok()
            }
            _ => unreachable!("only the three tools above are generated"),
        };
        if !ok {
            return None;
        }
        Some(value)
    }
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
        // Beta-audit P0.4: the invocations carry a REAL session id —
        // the run's primary session when it has one. A fabricated id
        // (the old `diagnose-focus-<target>` shape) targeted no session
        // at all; no id is better than a wrong id (the caller fills
        // their own), and a real one makes the step genuinely
        // ready-to-send.
        let suggested_next_observations =
            next_observations(&finding, sessions.first().map(String::as_str));
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
/// edit instruction. Each step names its MCP tool and exact arguments
/// (finding 23): the prose describes, the structure executes.
///
/// Beta-audit P0.4: every invocation is BUILT from the real MCP
/// parameter types and carries the caller's REAL session id — no
/// fabricated ids, no grammar the tools don't accept, no claims about
/// what a key does (Tab "targets a control" only when a proven focus
/// route says so, and none is available here, so the control steps
/// observe instead of drive).
fn next_observations(
    finding: &crate::audit::Finding,
    session_id: Option<&str>,
) -> Vec<NextObservation> {
    let mut out = Vec::new();
    for e in finding.evidence.iter() {
        let Some(target) = e.target.as_deref() else {
            continue;
        };
        match e.kind {
            crate::audit::EvidenceKind::Control => {
                // Observe the control's live facts (identity, affordances,
                // clipping, contract verdict) — target-aware WITHOUT
                // claiming a focus route: sending Tab "at" a control is
                // only meaningful when a proven focus path reaches it,
                // which this static finding does not establish.
                let observe = ToolInvocation::Observe(crate::mcp::params::TuiObserveParams {
                    mode: Some(crate::mcp::params::Known::Known(
                        crate::mcp::params::ObserveMode::Inspect,
                    )),
                    idle_ms: None,
                    id: session_id.map(String::from),
                    consumer: None,
                    query: None,
                    text: None,
                    since_seq: None,
                    until_seq: None,
                    limit: None,
                    event_types: None,
                    // mode=inspect targets the CURRENT frame; a specific
                    // control target would need a proven control id, which
                    // a static finding's string is not.
                    target: None,
                });
                if let Some(args) = observe.to_arguments() {
                    out.push(NextObservation {
                        suggestion: format!(
                            "inspect the current frame with tui_observe mode=inspect and read control '{target}'s facts (identity, clipping state, affordances, contract verdict)"
                        ),
                        rationale: "a control-targeted inspect separates 'the control is absent' from 'the control is present but clipped/unresponsive' — without claiming any focus route".into(),
                        tool: Some("tui_observe".into()),
                        arguments: args,
                    });
                }
                // A drift probe (stimulus omitted) on the live session:
                // pure observation — settled frame + material changes —
                // with no keys sent and no focus-route claim. Use this
                // to check whether the finding's state persists.
                let drift = ToolInvocation::Probe(crate::mcp::params::TuiProbeParams {
                    stimulus: None,
                    completion: None,
                    capture: None,
                    text: None,
                    watch: None,
                    quiet_ms: None,
                    budget_ms: None,
                    id: session_id.map(String::from),
                });
                if let Some(args) = drift.to_arguments() {
                    out.push(NextObservation {
                        suggestion: "run a drift probe (tui_probe with no stimulus) to capture the settled frame and watched material changes".to_string(),
                        rationale: "a stimulus-free probe confirms whether the finding's state is stable or transient, and records what moved — without driving anything".into(),
                        tool: Some("tui_probe".into()),
                        arguments: args,
                    });
                }
                out.push(NextObservation {
                    suggestion: "read the source loci in source_refs (opening with the attested ones) before editing anything".to_string(),
                    rationale: "provenance-tiered loci point at where the evidence was earned; correlated loci are leads, not cause sites".into(),
                    tool: None,
                    arguments: serde_json::json!({}),
                });
            }
            crate::audit::EvidenceKind::Region => {
                // Two-size compare: resize, observe, resize back, observe —
                // the caller re-runs the region check on both frames. The
                // resize carries the REAL session id; completion defaults
                // apply (resize settles fast; no fabricated completion).
                let resize = ToolInvocation::Act(crate::mcp::params::TuiActRequest::Resize(
                    crate::mcp::params::ResizePayload {
                        cols: 120,
                        rows: 40,
                        common: crate::mcp::params::ActCommon {
                            no_wait: None,
                            completion: None,
                            wait_ms: None,
                            settle_budget_ms: None,
                            id: session_id.map(String::from),
                            guard: None,
                        },
                    },
                ));
                if let Some(args) = resize.to_arguments() {
                    out.push(NextObservation {
                        suggestion: format!("resize to 120x40 (tui_act), tui_observe, and compare region '{target}' against the current frame — restore the size afterwards"),
                        rationale: "a region complaint that moves with size is layout math; one that persists across sizes is content or styling".into(),
                        tool: Some("tui_act".into()),
                        arguments: args,
                    });
                }
            }
            _ => {
                // Beta-audit P0.4: the old suggestion pointed at
                // `tui_run action=context`, which returns the capability
                // REGISTRY, not the transaction ledger. The truthful
                // surface for "what interactions preceded this" is the
                // observe history mode.
                let history = ToolInvocation::Observe(crate::mcp::params::TuiObserveParams {
                    mode: Some(crate::mcp::params::Known::Known(
                        crate::mcp::params::ObserveMode::History,
                    )),
                    idle_ms: None,
                    id: session_id.map(String::from),
                    consumer: None,
                    query: None,
                    text: None,
                    since_seq: Some(0),
                    until_seq: None,
                    limit: None,
                    event_types: None,
                    target: None,
                });
                if let Some(args) = history.to_arguments() {
                    out.push(NextObservation {
                        suggestion: format!("read the session's terminal event history (tui_observe mode=history) around the finding's evidence target '{target}' — the events that preceded the finding often name its trigger"),
                        rationale: "the interaction that preceded the observation often names the trigger the finding only implies".into(),
                        tool: Some("tui_observe".into()),
                        arguments: args,
                    });
                }
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
            kind: crate::audit::FindingKind::Defect,
            id: "CLIP-001".into(),
            rule_id: None,
            severity: crate::audit::Severity::Error,
            category: crate::audit::Category::Clipping,
            summary: "Save button clipped at right edge".into(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Control,
                "button/save",
                "clipped",
            )],
            confidence: 0.9,
            reproduction: repro.map(String::from),
            source_refs: refs,
            occurrence_id: None,
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
        // Finding 23: each observation carries its invocation structurally —
        // tool name + ready-to-send arguments — so no prose parsing is
        // needed to act.
        for obs in &ctx.suggested_next_observations {
            match &obs.tool {
                Some(tool) => {
                    assert!(
                        tool.starts_with("tui_"),
                        "tool names the MCP surface: {tool}"
                    );
                    assert!(
                        obs.arguments.is_object(),
                        "arguments are a JSON object: {}",
                        obs.arguments
                    );
                }
                None => assert!(
                    obs.arguments.is_object(),
                    "human-space steps carry an empty object"
                ),
            }
        }
        // Beta-audit P0.4: the probe step (if any) is a DRIFT probe —
        // no stimulus at all, never the old fabricated nested grammar
        // `{"stimulus":{"action":{"kind":...}}}` no tool accepts.
        if let Some(probe) = ctx
            .suggested_next_observations
            .iter()
            .find(|o| o.tool.as_deref() == Some("tui_probe"))
        {
            assert!(
                probe.arguments.get("stimulus").is_none() || probe.arguments["stimulus"].is_null(),
                "generated probes are stimulus-free (no focus-route claims): {}",
                probe.arguments
            );
            assert_eq!(
                probe.arguments["id"],
                serde_json::json!("s1"),
                "the REAL session id rides the invocation: {}",
                probe.arguments
            );
        }
        let serialized = serde_json::to_value(&ctx).unwrap();
        let first = &serialized["suggested_next_observations"][0];
        assert!(
            first.get("tool").is_some() && first.get("arguments").is_some(),
            "tool + arguments ride the wire: {first}"
        );
    }

    /// Beta-audit P0.4's core contract: EVERY generated invocation
    /// deserializes into its target tool's REAL request type. The old
    /// generator emitted `{"action":{"kind":"key"}}` (wrong grammar),
    /// fabricated session ids, and `tui_run context` misdescribed as
    /// the ledger — all of which deserialize fine as JSON but are
    /// rejected by the tools. This test fails if any generated step
    /// stops round-tripping.
    #[test]
    fn every_generated_invocation_round_trips_through_the_real_request_type() {
        // Control-kind finding (observe inspect + drift probe steps).
        let f = finding_with(None, vec![]);
        let ctx = DiagnosticContext::assemble(f, "run-1", vec!["sess-real".into()], |_| None)
            .expect("context");
        for obs in &ctx.suggested_next_observations {
            let (tool, args) = match (&obs.tool, obs.arguments.is_null()) {
                (Some(t), false) => (t.as_str(), obs.arguments.clone()),
                _ => continue,
            };
            let ok = match tool {
                "tui_observe" => {
                    serde_json::from_value::<crate::mcp::params::TuiObserveParams>(args.clone())
                        .is_ok()
                }
                "tui_probe" => {
                    serde_json::from_value::<crate::mcp::params::TuiProbeParams>(args.clone())
                        .is_ok()
                }
                "tui_act" => {
                    serde_json::from_value::<crate::mcp::params::TuiActRequest>(args.clone())
                        .is_ok()
                }
                other => panic!("unknown generated tool: {other}"),
            };
            assert!(
                ok,
                "generated {tool} invocation must deserialize into its real request type: {args}"
            );
        }
    }

    /// The audit's named defects, pinned individually:
    /// - no fabricated `diagnose-*` session ids anywhere;
    /// - no nested `action.kind` grammar;
    /// - no `tui_run context` misdescribed as the ledger;
    /// - resize/act steps carry the real session id.
    #[test]
    fn no_fabricated_ids_no_invalid_grammar_no_misdescribed_surfaces() {
        // Region-kind finding exercises the resize (act) step.
        let mut f = finding_with(None, vec![]);
        f.evidence = vec![EvidenceRef::point(
            EvidenceKind::Region,
            "sidebar/left",
            "clipped",
        )];
        let ctx = DiagnosticContext::assemble(f, "run-1", vec!["sess-real".into()], |_| None)
            .expect("context");
        for obs in &ctx.suggested_next_observations {
            let args = &obs.arguments;
            let text = args.to_string();
            assert!(
                !text.contains("diagnose-"),
                "no fabricated session ids: {text}"
            );
            assert!(
                args.get("action").map(|a| a.is_string()).unwrap_or(true),
                "action is a STRING selector (canonical grammar), never an object: {text}"
            );
            if obs.tool.as_deref() == Some("tui_act") {
                assert_eq!(
                    args["id"],
                    serde_json::json!("sess-real"),
                    "act steps carry the REAL session id: {text}"
                );
            }
            if obs.tool.as_deref() == Some("tui_observe") {
                assert_eq!(
                    args["id"],
                    serde_json::json!("sess-real"),
                    "observe steps carry the REAL session id: {text}"
                );
            }
            // The history suggestion names the observe surface, not
            // tui_run context.
            assert!(
                obs.tool.as_deref() != Some("tui_run"),
                "tui_run is never generated (its context action is the registry, not the ledger)"
            );
        }
    }

    /// No session known at assembly: steps omit `id` entirely rather
    /// than inventing one. A wrong id is worse than a missing id —
    /// the caller fills their own real one.
    #[test]
    fn steps_without_session_provenance_omit_the_id_field() {
        let f = finding_with(None, vec![]);
        let ctx = DiagnosticContext::assemble(f, "run-1", vec![], |_| None).expect("context");
        for obs in &ctx.suggested_next_observations {
            if obs.tool.is_some() {
                assert!(
                    obs.arguments.get("id").is_none() || obs.arguments["id"].is_null(),
                    "no fabricated ids — omit id when no session is known: {}",
                    obs.arguments
                );
            }
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
