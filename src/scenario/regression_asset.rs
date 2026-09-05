//! Regression-asset generation (finding 39).
//!
//! From a finding's OWN evidence and reproduction, synthesize the regression
//! assets that encode "this screen state must not regress" — a scenario,
//! an assertion precondition, an optional contract rule, a viewport case.
//!
//! Honesty contract — mirrored by the MCP arm but enforced HERE so no
//! caller can emit a phantom asset:
//!
//!   * every asset is stamped `generated`, `inferred`, `requires_review`;
//!   * a kind is only produced when the finding's evidence can justify it
//!     (the generator decides availability, the handler never overrides);
//!   * nothing here drives the app or records new evidence — generation is
//!     a pure read-and-synthesize pass over run-held state;
//!   * the scenario asset is built from the finding's own reproduction
//!     when present, else from the run's transaction ledger toward the
//!     evidence target — and only from acts that already happened
//!     (`origin=act`) or recorded scenario replay, never from guesses.

use crate::audit::{EvidenceKind, Finding};
use serde::{Deserialize, Serialize};

/// Which regression-asset kinks the generator can emit (finding 39).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionAssetKind {
    /// A replayable scenario (act/assert) reproducing the finding path.
    Scenario,
    /// A state-precondition assertion pinning the evidence target.
    Assertion,
    /// An OPTIONAL contract component rule for the evidence target.
    ContractRule,
    /// A viewport/resize case the finding's evidence cites.
    ViewportCase,
}

impl RegressionAssetKind {
    /// Stable wire slug.
    pub fn as_str(&self) -> &'static str {
        match self {
            RegressionAssetKind::Scenario => "scenario",
            RegressionAssetKind::Assertion => "assertion",
            RegressionAssetKind::ContractRule => "contract_rule",
            RegressionAssetKind::ViewportCase => "viewport_case",
        }
    }

    /// Whether the finding's evidence can justify an asset of this kind.
    pub fn available_for(&self, finding: &Finding) -> bool {
        match self {
            RegressionAssetKind::Scenario => {
                finding.reproduction.is_some()
                    || finding
                        .evidence
                        .iter()
                        .any(|e| matches!(e.kind, EvidenceKind::Transaction))
            }
            // An assertion needs a concrete target to pin (control/region/
            // frame), not a bare free-form point.
            RegressionAssetKind::Assertion => finding.evidence.iter().any(|e| {
                e.target.is_some()
                    && matches!(
                        e.kind,
                        EvidenceKind::ScreenSnapshot
                            | EvidenceKind::Control
                            | EvidenceKind::Region
                            | EvidenceKind::Frame
                    )
            }),
            // A contract rule is about a CONTROL, not a frame hash.
            RegressionAssetKind::ContractRule => finding.evidence.iter().any(|e| {
                e.target
                    .as_deref()
                    .is_some_and(|t| !t.starts_with("structure:v1"))
                    && matches!(e.kind, EvidenceKind::Control | EvidenceKind::Region)
            }),
            // A viewport case needs an explicit size from the evidence
            // detail (the frame-diff/clipping/resize families carry one).
            RegressionAssetKind::ViewportCase => finding.evidence.iter().any(|e| {
                e.detail.get("cols").is_some()
                    || e.detail.get("rows").is_some()
                    || matches!(e.kind, EvidenceKind::Diff)
            }),
        }
    }
}

/// One synthesized regression asset (finding 39).
#[derive(Debug, Clone, Serialize)]
pub struct RegressionAsset {
    /// Which kind this is.
    pub kind: RegressionAssetKind,
    /// Stable asset id within this run (`rag-<uuid>`).
    pub id: String,
    /// Human-readable name, derived from the rule.
    pub name: String,
    /// PROVENANCE: which evidence target(s) + the finding this was built
    /// from. Never a guess — every field traces back to run-held state.
    pub provenance: String,
    /// ALWAYS true for a generated asset: it was inferred from evidence
    /// after the fact, not authored against intent. A reviewer must
    /// confirm it encodes the true regression before it is trusted.
    pub generated: bool,
    /// ALWAYS true: the exact shape is my inference, not an app assertion.
    pub inferred: bool,
    /// The reviewer gate. A generated asset must not be treated as paid-up
    /// truth until a human has read it.
    pub requires_review: bool,
    /// The asset body, shaped per kind (see the generator).
    pub body: serde_json::Value,
}

impl RegressionAsset {
    /// The scenario-form body, parsed back out (owned) for persistence. For
    /// the scenario kind the body IS the serialized Scenario.
    pub fn as_scenario(&self) -> Option<crate::scenario::model::Scenario> {
        if self.kind != RegressionAssetKind::Scenario {
            return None;
        }
        serde_json::from_value(self.body.clone()).ok()
    }
}

/// The raw material the generator draws from, so it stays a pure function
/// of run-held state and never needs to drive the app.
pub struct AssetContext<'a> {
    /// Resolve a scenario by id (the finding's own `reproduction`).
    pub load_scenario: &'a dyn Fn(&str) -> Option<crate::scenario::model::Scenario>,
    /// The run's transaction ledger, newest-first, for scenario assembly.
    pub transactions: &'a [crate::run::TransactionRecord],
}

/// Generate every regression-asset kind the finding's evidence can justify,
/// filtered by `only` when the caller wants a specific kind. Empty (with no
/// error) when the finding has no reproduction, no transaction evidence, and
/// no concrete target — the caller surfaces that as "nothing synthesized,
/// here is the reason", never as "here is an empty shell".
pub fn generate(
    finding: &Finding,
    ctx: &AssetContext<'_>,
    only: Option<RegressionAssetKind>,
) -> Vec<RegressionAsset> {
    let rule = finding
        .rule_id
        .clone()
        .unwrap_or_else(|| finding.id.clone());
    let target = finding
        .evidence
        .iter()
        .filter_map(|e| e.target.clone())
        .next()
        .unwrap_or_default();

    let mut out = Vec::new();
    let push =
        |out: &mut Vec<RegressionAsset>, kind: RegressionAssetKind, body: serde_json::Value| {
            if only.is_some_and(|k| k != kind) {
                return;
            }
            out.push(RegressionAsset {
                kind,
                id: format!("rag-{}", uuid::Uuid::new_v4().simple()),
                name: format!("{}-{}", kind.as_str(), rule),
                provenance: format!("finding:{id}:{rule}:target:{target}", id = finding.id),
                generated: true,
                inferred: true,
                requires_review: true,
                body,
            });
        };

    // ── scenario ──
    if RegressionAssetKind::Scenario.available_for(finding) {
        let scenario = build_scenario(finding, ctx, &rule, &target);
        if let Some(sc) = scenario {
            push(
                &mut out,
                RegressionAssetKind::Scenario,
                serde_json::to_value(sc).unwrap_or_default(),
            );
        }
    }

    // ── assertion ──
    if RegressionAssetKind::Assertion.available_for(finding) {
        let assertion = build_assertion(finding, &target);
        push(&mut out, RegressionAssetKind::Assertion, assertion);
    }

    // ── contract_rule ──
    if RegressionAssetKind::ContractRule.available_for(finding) {
        let rule_asset = build_contract_rule(finding);
        push(&mut out, RegressionAssetKind::ContractRule, rule_asset);
    }

    // ── viewport_case ──
    if RegressionAssetKind::ViewportCase.available_for(finding) {
        let vp = build_viewport_case(finding);
        if let Some(vp) = vp {
            push(&mut out, RegressionAssetKind::ViewportCase, vp);
        }
    }

    out
}

/// Build a replayable scenario: from the finding's own reproduction when
/// present, else from the run's transaction ledger toward the evidence
/// target (the acts that actually reached it, in run order). Never invents
/// steps that did not happen.
fn build_scenario(
    finding: &Finding,
    ctx: &AssetContext<'_>,
    rule: &str,
    target: &str,
) -> Option<crate::scenario::model::Scenario> {
    // Preferred: the finding's recorded reproduction, verbatim.
    if let Some(repro) = finding.reproduction.as_deref() {
        if let Some(sc) = (ctx.load_scenario)(repro) {
            return Some(clone_marked_generated(sc, finding, rule));
        }
    }
    // Fallback: the run's transaction ledger, filtered to interactions that
    // actually touched the evidence target, new→old, then reversed to run
    // order and taken as act steps. Only `origin=act`/`scenario`/`intent`
    // entries are eligible — exploration-style bulk input is NOT a
    // reproducible regression path.
    let mut scn = crate::scenario::model::Scenario::new(format!("regression-{}", rule));
    scn.metadata = Some(crate::scenario::model::ScenarioMetadata {
        description: Some(format!(
            "generated regression asset from finding '{}' target '{}' (requires review before use)",
            rule, target
        )),
        tags: vec![
            "generated".into(),
            "regression".into(),
            "requires_review".into(),
        ],
        created_at: None,
        source: Some(format!("tui_lab/regression_asset/finding/{}", finding.id)),
    });
    // If the target is a frame hash, the ledger has no control-scoped rows —
    // only a reproduction can carry it. Refuse empty.
    if target.starts_with("structure:v1") {
        return None;
    }
    let mut acts: Vec<crate::run::TransactionRecord> = ctx
        .transactions
        .iter()
        .filter(|t| {
            t.action != "wait"
                && t.origin
                    .as_deref()
                    .is_some_and(|o| matches!(o, "act" | "scenario" | "intent"))
                && (t.after_structure.contains(target) || t.before_structure.contains(target))
        })
        .cloned()
        .collect();
    if acts.is_empty() {
        return None;
    }
    acts.reverse(); // oldest→newest in replay order
    for t in acts {
        // Persisted action may be redacted — only carry full actions.
        if let Some(pa) = &t.persisted_action {
            if let Ok(params) = serde_json::to_value(pa) {
                scn = scn.act(params);
            }
        } else {
            // A non-secret action: the action name is the canonical act
            // parameter (key/type/mouse_click) via the `action` field — NOT
            // `kind`, which is reserved for the step discriminator. Rebuild
            // the minimal invocation that produced this transition without
            // leaking payload we never stored.
            scn = scn.act(serde_json::json!({ "action": t.action }));
        }
    }
    if scn.steps.is_empty() {
        return None;
    }
    Some(scn)
}

/// Clone a reproduction scenario with the generated-review markers (the
/// underlying recorded steps are preserved verbatim — generation only adds
/// provenance).
fn clone_marked_generated(
    mut sc: crate::scenario::model::Scenario,
    finding: &Finding,
    rule: &str,
) -> crate::scenario::model::Scenario {
    let src = format!("tui_lab/regression_asset/finding/{}", finding.id);
    let meta = sc
        .metadata
        .get_or_insert_with(|| crate::scenario::model::ScenarioMetadata {
            description: None,
            tags: Vec::new(),
            created_at: None,
            source: None,
        });
    if meta.source.as_deref() != Some(src.as_str()) {
        meta.source = Some(src);
        meta.tags.push("generated".into());
        meta.tags.push("requires_review".into());
        meta.description = Some(format!(
            "generated regression asset from finding '{}' rule '{}' (reproduction; requires review before use)",
            finding.id, rule
        ));
    }
    sc
}

/// Build a state-pin assertion from the evidence target (a structure hash
/// or control id), carried as a StepExpect-style precondition so a reviewer
/// sees exactly what state the regression relies on.
fn build_assertion(finding: &Finding, target: &str) -> serde_json::Value {
    // The pin: prefer a `structure_hash` the evidence carries explicitly;
    // otherwise the target's own id (control/region/frame) is the pin.
    let struct_hash: String = finding
        .evidence
        .iter()
        .filter_map(|e| e.detail.get("structure_hash").and_then(|v| v.as_str()))
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| target.to_string());
    serde_json::json!({
        "target": target,
        "check": "state_precondition",
        "structure_hash": struct_hash,
        "note": "generated assertion — pin the evidence target's state so the screen does not regress past it. Requires review: confirm it encodes the true invariant.",
    })
}

/// Build an OPTIONAL contract rule: a `control_exists`-style component
/// citation for the evidence target. Mirrors the F37 scaffold discipline —
/// never `required` from observation alone.
fn build_contract_rule(finding: &Finding) -> serde_json::Value {
    let target = finding
        .evidence
        .iter()
        .filter_map(|e| e.target.clone())
        .next()
        .unwrap_or_default();
    serde_json::json!({
        "component": {
            "name": target.clone(),
            "role": "control_exists",
            "required": false,
            "note": "generated from a finding target; observationally present, not contractually required. Requires review before promoting to required.",
        }
    })
}

/// Build a viewport/resize case from the evidence detail's cited size (or
/// from a diff family). `None` when the evidence carries no explicit size.
fn build_viewport_case(finding: &Finding) -> Option<serde_json::Value> {
    for e in &finding.evidence {
        let cols = e.detail.get("cols").and_then(|v| v.as_u64());
        let rows = e.detail.get("rows").and_then(|v| v.as_u64());
        if let (Some(c), Some(r)) = (cols, rows) {
            return Some(serde_json::json!({
                "resize": { "cols": c, "rows": r },
                "kind": "viewport_case",
                "check": "re-render at this size and confirm the finding does not regress",
                "note": "generated viewport case from the evidence's cited size. Requires review.",
            }));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{Category, EvidenceKind, EvidenceRef, Severity};

    fn finding(id: &str, ev: Vec<EvidenceRef>, reproduction: Option<String>) -> Finding {
        Finding {
            id: id.into(),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::Focus,
            summary: format!("{id} summary"),
            evidence: ev,
            confidence: 0.7,
            reproduction,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    fn screen_ev(target: &str, cols: u64, rows: u64) -> EvidenceRef {
        let mut e = EvidenceRef::screen(target, "screen");
        e.detail = serde_json::json!({ "cols": cols, "rows": rows });
        e
    }

    fn no_scn(_: &str) -> Option<crate::scenario::model::Scenario> {
        None
    }

    fn ctx<'a>(
        load: &'a dyn Fn(&str) -> Option<crate::scenario::model::Scenario>,
        txs: &'a [crate::run::TransactionRecord],
    ) -> AssetContext<'a> {
        AssetContext {
            load_scenario: load,
            transactions: txs,
        }
    }

    #[test]
    fn reproduction_scenario_is_marked_and_preserved() {
        // A finding with a recorded reproduction yields a scenario asset
        // (and a state-pin assertion for the screen snapshot it cites).
        let sc = crate::scenario::model::Scenario::new("repro")
            .act(serde_json::json!({ "action": "key", "key": "Enter" }));
        let f = finding(
            "F-REPRO",
            vec![EvidenceRef::point(
                EvidenceKind::ScreenSnapshot,
                "structure:v1:abc",
                "s",
            )],
            Some("scn-repro".into()),
        );
        let get = |_: &str| Some(sc.clone());
        let assets = generate(&f, &ctx(&get, &[]), None);
        // scenario (from reproduction) + assertion (state pin). No contract
        // rule (frame hash, not a control) and no viewport case (no size).
        let kinds: Vec<_> = assets.iter().map(|a| a.kind).collect();
        assert!(
            kinds.contains(&RegressionAssetKind::Scenario),
            "scenario justified by reproduction: {assets:?}"
        );
        assert!(
            kinds.contains(&RegressionAssetKind::Assertion),
            "assertion pins the screen target: {assets:?}"
        );
        let scen = assets
            .iter()
            .find(|a| a.kind == RegressionAssetKind::Scenario)
            .unwrap();
        assert!(scen.generated && scen.inferred && scen.requires_review);
        let body = scen.as_scenario().expect("scenario body");
        assert_eq!(body.steps.len(), 1, "the reproduction's step is preserved");
        assert!(body.metadata.is_some(), "generation tags metadata");
        assert_eq!(
            scen.provenance,
            "finding:F-REPRO:F-REPRO:target:structure:v1:abc"
        );
    }

    #[test]
    fn control_target_yields_contract_rule_and_assertion() {
        // A control/region target justifies assertion + contract_rule.
        let f = finding(
            "F-CTRL",
            vec![EvidenceRef::point(
                EvidenceKind::Control,
                "button/save",
                "s",
            )],
            None,
        );
        let assets = generate(&f, &ctx(&no_scn, &[]), None);
        let kinds: Vec<_> = assets.iter().map(|a| a.kind.as_str()).collect();
        assert!(kinds.contains(&"assertion"), "{assets:?}");
        assert!(kinds.contains(&"contract_rule"), "{assets:?}");
        // No scenario: no reproduction and no transaction evidence.
        assert!(!kinds.contains(&"scenario"), "{assets:?}");
        for a in &assets {
            assert!(a.generated && a.inferred && a.requires_review, "{a:?}");
        }
        let cr = assets
            .iter()
            .find(|a| a.kind == RegressionAssetKind::ContractRule)
            .unwrap();
        assert_eq!(cr.body["component"]["required"], false);
    }

    #[test]
    fn bare_frame_no_repro_no_scenario() {
        // A bare screen-point finding with no reproduction/transactions and
        // no control target: NO scenario, NO contract rule, NO viewport case
        // — but an assertion pinning the frame hash is still honest.
        let f = finding(
            "F-EMPTY",
            vec![EvidenceRef::point(
                EvidenceKind::ScreenSnapshot,
                "structure:v1:xyz",
                "s",
            )],
            None,
        );
        let assets = generate(&f, &ctx(&no_scn, &[]), None);
        let kinds: Vec<_> = assets.iter().map(|a| a.kind).collect();
        assert!(
            !kinds.contains(&RegressionAssetKind::Scenario),
            "{assets:?}"
        );
        assert!(
            !kinds.contains(&RegressionAssetKind::ContractRule),
            "{assets:?}"
        );
        assert!(
            !kinds.contains(&RegressionAssetKind::ViewportCase),
            "{assets:?}"
        );
        // The only honest asset: a state-pin assertion on the frame hash.
        assert_eq!(kinds, vec![RegressionAssetKind::Assertion], "{assets:?}");
    }

    #[test]
    fn viewport_case_from_detail() {
        let f = finding("F-VP", vec![screen_ev("button/x", 120, 40)], None);
        let assets = generate(&f, &ctx(&no_scn, &[]), None);
        assert!(
            assets
                .iter()
                .any(|a| a.kind == RegressionAssetKind::ViewportCase),
            "{assets:?}"
        );
    }
}
