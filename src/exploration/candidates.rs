//! Novel-action candidates for Hermes-guided exploration (spec section 4.3).
//!
//! Wave D item 34: candidates are *evidential*. The old `suggest` claimed
//! "focused control not yet activated" with no graph to consult — an
//! untestable assertion. Every candidate now cites its evidence:
//!
//! * `graph`: the state graph knows whether this action was ever taken out
//!   of the current state (`times_taken: 0` is a real observation);
//! * `history`: the action sequence actually executed this exploration;
//! * `contract`: the design contract's declared keybindings (declared but
//!   never exercised = a coverage claim, not a guess);
//! * `affordance`: an on-screen hint naming the key;
//! * `coverage`: which control IDs the interaction has reached.
//!
//! A candidate without evidence is not emitted. `allowed_risk` gates every
//! suggestion so an explorer never gets "activate Delete Database" offered
//! as a safe next step (item 33).

use crate::exploration::state_graph::{StateGraph, StateId};
use crate::intent::{classify_risk, ActionRisk};
use crate::screen::ScreenState;
use crate::semantic::SemanticScreen;
use serde_json::json;

/// Everything a candidate generator may cite as evidence (Wave D item 34).
/// All fields are optional so partial contexts still work, but every
/// emitted candidate names which parts it used.
pub struct CandidateContext<'a> {
    /// The run's state graph: what was actually executed, keyed by layered
    /// state identity.
    pub state_graph: &'a StateGraph,
    /// Identity of the state the explorer is in right now.
    pub current: StateId,
    /// Ordered action names executed so far (most recent last).
    pub action_history: &'a [String],
    /// Control IDs the interaction has reached so far (focused/activated).
    pub coverage: &'a [String],
    /// The app's declared keybinding contract, when one exists.
    pub contract: Option<&'a crate::design::schema::DesignContract>,
    /// Highest risk the caller accepts. Candidates above this are filtered
    /// out entirely — never offered with a warning, just absent.
    pub allowed_risk: ActionRisk,
}

impl<'a> CandidateContext<'a> {
    /// A context for screens/tests with no history: everything is novel by
    /// construction, and only safe+mutating actions are allowed.
    pub fn fresh(state_graph: &'a StateGraph, current: StateId) -> Self {
        CandidateContext {
            state_graph,
            current,
            action_history: &[],
            coverage: &[],
            contract: None,
            allowed_risk: ActionRisk::Mutating,
        }
    }
}

/// One suggested next action with its evidence trail.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Candidate {
    pub action: serde_json::Value,
    /// Why this candidate: one or more evidence citations, each naming its
    /// source (`graph`, `history`, `contract`, `affordance`, `coverage`).
    pub reasons: Vec<Evidence>,
    pub risk: ActionRisk,
    /// Control this acts on, when the candidate is target-directed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
}

/// A single evidential claim (item 34).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Evidence {
    /// Where the claim comes from: graph | history | contract | affordance | coverage.
    pub source: &'static str,
    /// The claim itself ("never taken from this state", "declared in contract, unexercised").
    pub claim: String,
    /// Structured detail (counts, key names, control ids).
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

impl Evidence {
    fn graph(claim: impl Into<String>, detail: serde_json::Value) -> Self {
        Evidence { source: "graph", claim: claim.into(), detail }
    }
    fn contract(claim: impl Into<String>, detail: serde_json::Value) -> Self {
        Evidence { source: "contract", claim: claim.into(), detail }
    }
    fn affordance(claim: impl Into<String>, detail: serde_json::Value) -> Self {
        Evidence { source: "affordance", claim: claim.into(), detail }
    }
    fn coverage(claim: impl Into<String>, detail: serde_json::Value) -> Self {
        Evidence { source: "coverage", claim: claim.into(), detail }
    }
}

/// Suggest novel next actions for the current state. Deterministic.
pub fn suggest(
    _screen: &ScreenState,
    sem: &SemanticScreen,
    ctx: &CandidateContext,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // ── 1) Traversal actions the graph has never seen from this state ──
    for (name, key_json) in [
        ("tab", json!({ "action": "key", "key": "tab" })),
        (
            "shift+tab",
            json!({ "action": "key", "key": "shift+tab" }),
        ),
        ("down", json!({ "action": "key", "key": "down" })),
        ("up", json!({ "action": "key", "key": "up" })),
        ("enter", json!({ "action": "key", "key": "enter" })),
        ("escape", json!({ "action": "key", "key": "escape" })),
    ] {
        let taken = ctx.state_graph.action_taken_from(&ctx.current, name);
        // Only actions never taken from this state — that is the graph's
        // claim, verifiable from the edge list.
        if taken == 0 {
            out.push(Candidate {
                action: key_json,
                reasons: vec![Evidence::graph(
                    format!("'{name}' has never been executed from this state"),
                    json!({ "times_taken_from_state": 0, "state": ctx.current.as_str() }),
                )],
                risk: if name == "enter" { ActionRisk::Mutating } else { ActionRisk::Safe },
                control_id: None,
            });
        }
    }

    // ── 2) Contract keybindings declared but never exercised ──
    if let Some(contract) = ctx.contract {
        for kb in &contract.keybindings {
            for key in &kb.keys {
                let taken = ctx.state_graph.action_taken_from(&ctx.current, key);
                if taken == 0 && !ctx.action_history.iter().any(|a| a == key) {
                    // Risk of an unexercised declared binding is unknown —
                    // treat as mutating so it cannot slip past a safe gate.
                    out.push(Candidate {
                        action: json!({ "action": "key", "key": key }),
                        reasons: vec![Evidence::contract(
                            format!("'{key}' is declared for '{} 'in the contract but never exercised in this run", kb.action.trim_end()),
                            json!({ "declared_action": kb.action, "key": key, "times_exercised": 0 }),
                        )],
                        risk: ActionRisk::Mutating,
                        control_id: None,
                    });
                }
            }
        }
    }

    // ── 3) On-screen affordances not yet exercised ──
    for aff in &sem.affordances {
        let Some(key) = (match &aff.invocation {
            crate::semantic::affordance::Invocation::Key { key } => Some(key.clone()),
            _ => None,
        }) else {
            continue;
        };
        let taken = ctx.state_graph.action_taken_from(&ctx.current, &key);
        if taken == 0 {
            out.push(Candidate {
                action: json!({ "action": "key", "key": key }),
                reasons: vec![Evidence::affordance(
                    format!("screen labels '{key}' for '{}' and it has not been tried from this state", aff.action),
                    json!({ "hint_text": aff.hint_text, "visibility": aff.visibility }),
                )],
                risk: classify_risk(
                    &crate::intent::ActionVerb::Activate,
                    aff.control_id
                        .as_deref()
                        .and_then(|id| sem.controls.iter().find(|c| c.id == id)),
                ),
                control_id: aff.control_id.clone(),
            });
        }
    }

    // ── 4) Controls interaction has never reached (coverage evidence) ──
    for c in &sem.controls {
        if !c.focusable {
            continue;
        }
        if !ctx.coverage.iter().any(|id| id == &c.id) {
            out.push(Candidate {
                action: json!({ "action": "focus_target", "target": { "by": "id", "id": c.id } }),
                reasons: vec![Evidence::coverage(
                    format!("control '{}' ('{}') has never held focus in this run", c.id, c.label),
                    json!({ "control_id": c.id, "label": c.label, "kind": c.kind }),
                )],
                risk: ActionRisk::Safe,
                control_id: Some(c.id.clone()),
            });
        }
    }

    // Deduplicate identical actions (graph + affordance sources overlap),
    // merging their evidence lists.
    let mut deduped: Vec<Candidate> = Vec::new();
    for c in out {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|e| e.action == c.action)
        {
            existing.reasons.extend(c.reasons);
            // The highest observed risk stands.
            if c.risk > existing.risk {
                existing.risk = c.risk;
            }
        } else {
            deduped.push(c);
        }
    }

    // Risk gate: candidates above the allowance are filtered entirely
    // (item 33) — an explorer with allowed_risk=safe never sees "enter".
    deduped.retain(|c| c.risk <= ctx.allowed_risk);

    // Stable order: risk ascending, then action JSON for determinism.
    deduped.sort_by(|a, b| {
        a.risk
            .cmp(&b.risk)
            .then_with(|| a.action.to_string().cmp(&b.action.to_string()))
    });
    deduped
}

/// Legacy 2-arg entry point (screen + semantic only): a fresh context over
/// an empty graph. Kept for tests and simple callers; production paths
/// build a real [`CandidateContext`].
pub fn suggest_fresh(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Candidate> {
    let empty = StateGraph::new(crate::exploration::state_graph::ExplorationBudget::default());
    let current = StateId::from_structure_hash(&screen.structure_hash);
    let ctx = CandidateContext {
        state_graph: &empty,
        current,
        action_history: &[],
        coverage: &[],
        contract: None,
        allowed_risk: ActionRisk::Mutating,
    };
    suggest(screen, sem, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::affordance::{Affordance, Invocation, Visibility};
    use crate::semantic::confidence::Confidence;
    use crate::semantic::controls::{Control, ControlBounds, ControlKind};
    use crate::semantic::focus::FocusInfo;

    fn screen() -> ScreenState {
        ScreenState {
            cols: 80,
            rows: 2,
            cursor: crate::screen::CursorState { x: 0, y: 0, visible: true },
            title: None,
            cells: Vec::new(),
            viewport_text: vec!["[ Save ]".into(), "q quit".into()],
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: "test-structure".into(),
            process: crate::screen::ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    fn sem_with(controls: Vec<Control>, affordances: Vec<Affordance>) -> SemanticScreen {
        SemanticScreen {
            cols: 80,
            rows: 2,
            regions: Vec::new(),
            controls,
            focus: FocusInfo::default(),
            relationships: Vec::new(),
            affordances,
            components: Vec::new(),
        }
    }

    fn button(id: &str, label: &str) -> Control {
        Control {
            id: id.into(),
            kind: ControlKind::Button,
            label: label.into(),
            value: None,
            bounds: ControlBounds { x: 0, y: 0, width: 8, height: 1 },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: Confidence::inferred(0.9, &["test"]),
            evidence: Vec::new(),
            source: "inferred".into(),
        }
    }

    fn aff(key: &str, action: &str) -> Affordance {
        Affordance {
            action: action.into(),
            control_id: None,
            invocation: Invocation::Key { key: key.into() },
            visibility: Visibility::Labeled,
            hint_text: Some("q quit".into()),
            confidence: Confidence::inferred(0.65, &["hint-line"]),
            source: "inferred".into(),
        }
    }

    /// Item 34, the core fix: "never activated" claims must come from the
    /// graph. With tab executed from this state, tab is NOT suggested and
    /// the graph evidence names zero counts.
    #[test]
    fn graph_evidence_drives_suggestions() {
        let s = screen();
        let sem = sem_with(vec![], vec![]);
        let mut g = StateGraph::new(crate::exploration::state_graph::ExplorationBudget::default());
        let current = StateId::from_structure_hash("test-structure");
        // The explorer already tried tab from this state: an identity whose
        // id() equals `current` (no interaction layer → content key).
        let id_self =
            crate::exploration::state_graph::StateIdentity::from_structure_content("test-structure");
        g.record_transition_identity(
            &id_self,
            &crate::exploration::state_graph::StateIdentity::from_parts("other"),
            "tab",
        );
        let ctx = CandidateContext {
            state_graph: &g,
            current,
            action_history: &[],
            coverage: &[],
            contract: None,
            allowed_risk: ActionRisk::Mutating,
        };
        let out = suggest(&s, &sem, &ctx);
        // tab was taken from this state → absent; every other traversal is
        // present with a zero-count graph citation.
        assert!(
            !out.iter().any(|c| c.action["key"] == "tab"),
            "tab was taken; must not be suggested: {:?}",
            out.iter().map(|c| c.action.to_string()).collect::<Vec<_>>()
        );
        let enter = out
            .iter()
            .find(|c| c.action["key"] == "enter")
            .expect("enter never taken → suggested");
        assert!(
            enter.reasons.iter().any(|r| r.source == "graph"
                && r.detail["times_taken_from_state"] == 0),
            "reason must cite the graph: {:?}",
            enter.reasons
        );
    }

    /// Item 33: the risk gate filters candidates above the allowance —
    /// silently (not offered with a warning, just absent).
    #[test]
    fn risk_gate_filters_candidates() {
        let s = screen();
        let sem = sem_with(vec![], vec![]);
        let g = StateGraph::new(crate::exploration::state_graph::ExplorationBudget::default());
        let ctx = CandidateContext {
            state_graph: &g,
            current: StateId::from_structure_hash("test-structure"),
            action_history: &[],
            coverage: &[],
            contract: None,
            allowed_risk: ActionRisk::Safe,
        };
        let out = suggest(&s, &sem, &ctx);
        assert!(
            out.iter().all(|c| c.risk <= ActionRisk::Safe),
            "safe-gated context must only offer safe candidates: {:?}",
            out.iter().map(|c| (c.action.to_string(), c.risk)).collect::<Vec<_>>()
        );
        assert!(
            !out.iter().any(|c| c.action["key"] == "enter"),
            "enter is mutating and must be filtered under allowed_risk=safe"
        );
    }

    /// Coverage evidence: a focusable control the interaction never reached
    /// is a candidate citing its ID.
    #[test]
    fn unreached_controls_cited_by_coverage() {
        let s = screen();
        let sem = sem_with(vec![button("button/save", "Save")], vec![]);
        let g = StateGraph::new(crate::exploration::state_graph::ExplorationBudget::default());
        let ctx = CandidateContext {
            state_graph: &g,
            current: StateId::from_structure_hash("test-structure"),
            action_history: &[],
            coverage: &["button/other".to_string()],
            contract: None,
            allowed_risk: ActionRisk::Mutating,
        };
        let out = suggest(&s, &sem, &ctx);
        let cov = out
            .iter()
            .find(|c| c.action["action"] == "focus_target")
            .expect("unreached control → focus candidate");
        assert_eq!(cov.control_id.as_deref(), Some("button/save"));
        assert!(
            cov.reasons.iter().any(|r| r.source == "coverage"),
            "reason must cite coverage: {:?}",
            cov.reasons
        );
        // Already-covered controls do not reappear.
        let ctx2 = CandidateContext {
            coverage: &["button/save".to_string()],
            ..CandidateContext::fresh(&g, StateId::from_structure_hash("test-structure"))
        };
        let out2 = suggest(&s, &sem, &ctx2);
        assert!(
            !out2.iter().any(|c| c.action["action"] == "focus_target"),
            "covered control must not be re-suggested"
        );
    }

    /// Affordance + contract evidence, and evidence merging when two
    /// sources propose the same key.
    #[test]
    fn affordance_and_contract_evidence() {
        let s = screen();
        let sem = sem_with(vec![], vec![aff("q", "quit")]);
        let contract = crate::design::schema::DesignContract {
            schema: crate::design::schema::ContractSchema {
                name: "t".into(),
                version: "1".into(),
            },
            viewports: vec![],
            keybindings: vec![crate::design::schema::Keybinding {
                action: "quit".into(),
                keys: vec!["q".into()],
            }],
            escape_closes_modal: true,
            reverse_tab_required: true,
            destructive_require_confirmation: true,
            volatile_patterns: vec![],
        };
        let g = StateGraph::new(crate::exploration::state_graph::ExplorationBudget::default());
        let ctx = CandidateContext {
            state_graph: &g,
            current: StateId::from_structure_hash("test-structure"),
            action_history: &[],
            coverage: &[],
            contract: Some(&contract),
            allowed_risk: ActionRisk::Mutating,
        };
        let out = suggest(&s, &sem, &ctx);
        let q = out
            .iter()
            .find(|c| c.action["key"] == "q")
            .expect("q never exercised → candidate");
        // Both the affordance source and the contract source apply; the
        // dedup merged their evidence.
        let sources: Vec<&str> = q.reasons.iter().map(|r| r.source).collect();
        assert!(sources.contains(&"affordance"), "{:?}", q.reasons);
        assert!(sources.contains(&"contract"), "{:?}", q.reasons);
    }
}
