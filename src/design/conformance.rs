//! Conformance checking (Wave E items 45–47): a [`ProjectContract`] against
//! a live session.
//!
//! Checks run in five groups, each contributing typed results:
//!
//! 1. **document** — static validation (oracles parse, regexes compile,
//!    names are unique). A document error is a contract-authoring bug, not
//!    an app failure.
//! 2. **component** — every declared component is found on the live screen
//!    (role matched against screen-level components, region kinds, and the
//!    semantic tree); declared `expect` oracles evaluate when present.
//! 3. **interaction** — the declared keys are sent through the canonical
//!    executor and the `expect` oracles are evaluated against the frame
//!    they produced.
//! 4. **layout** — declared viewports and layout constraints are resized
//!    to, and clipping is checked (reuses the resize-audit machinery).
//! 5. **behavior** — the flag-backed properties (`escape_closes_modal`,
//!    `reverse_tab_required`) are *proven* by driving the app, never
//!    assumed, and every top-level `oracles` entry is evaluated.
//!
//! The overall verdict is PASS only when nothing failed: FAIL for any
//! failed required check, WARN when only optional/nice-to-have checks
//! failed, PASS otherwise. Parse-level errors in the contract itself are
//! surfaced as `document` failures so a typo can never masquerade as an
//! app bug.

use super::oracle::{self, ActiveArgs, OracleOutcome};
use super::schema::{ComponentContract, InteractionContract, LayoutConstraint, ProjectContract};
use crate::audit::{EvidenceKind, EvidenceRef, Finding};
use crate::execution::{execute_act, CanonicalAction};
use crate::semantic::{self, node::Role};
use crate::session::state::Session;
use serde_json::json;

/// Severity of one conformance check result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Warn => "WARN",
            Verdict::Fail => "FAIL",
        }
    }
    /// Merge: a Fail dominates, then Warn.
    pub fn merge(self, other: Verdict) -> Verdict {
        if self == Verdict::Fail || other == Verdict::Fail {
            Verdict::Fail
        } else if self == Verdict::Warn || other == Verdict::Warn {
            Verdict::Warn
        } else {
            Verdict::Pass
        }
    }
}

/// One checked claim.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckResult {
    /// Check group: document | component | interaction | layout | behavior.
    pub group: &'static str,
    /// What was checked (component/interaction name, oracle expression…).
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
    /// Whether failing this result is contract-fatal (required components)
    /// or advisory (optional ones).
    pub required: bool,
}

impl CheckResult {
    fn pass(group: &'static str, name: impl Into<String>, detail: impl Into<String>) -> Self {
        CheckResult {
            group,
            name: name.into(),
            verdict: Verdict::Pass,
            detail: detail.into(),
            required: true,
        }
    }
    fn fail(group: &'static str, name: impl Into<String>, detail: impl Into<String>) -> Self {
        CheckResult {
            group,
            name: name.into(),
            verdict: Verdict::Fail,
            detail: detail.into(),
            required: true,
        }
    }
    fn warn(group: &'static str, name: impl Into<String>, detail: impl Into<String>) -> Self {
        CheckResult {
            group,
            name: name.into(),
            verdict: Verdict::Warn,
            detail: detail.into(),
            required: false,
        }
    }
    fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }
}

/// The full conformance report for one contract.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractReport {
    pub contract: String,
    pub version: String,
    /// PASS / WARN / FAIL for the whole contract.
    pub verdict: Verdict,
    pub results: Vec<CheckResult>,
    /// Number of app-driving actions the check performed (evidence for
    /// "this was proven, not assumed").
    pub driven_actions: u32,
}

impl ContractReport {
    /// Summarize for the MCP surface.
    pub fn summary(&self) -> serde_json::Value {
        let count = |v: Verdict| self.results.iter().filter(|r| r.verdict == v).count();
        json!({
            "contract": self.contract,
            "version": self.version,
            "verdict": self.verdict.as_str(),
            "driven_actions": self.driven_actions,
            "pass": count(Verdict::Pass),
            "warn": count(Verdict::Warn),
            "fail": count(Verdict::Fail),
        })
    }

    /// Failed oracles become evidence-backed findings so the contract path
    /// feeds the same finding ledger as the audits (item 49).
    pub fn findings(&self) -> Vec<Finding> {
        let mut out = Vec::new();
        for r in &self.results {
            if r.verdict == Verdict::Pass {
                continue;
            }
            let severity = match (r.verdict, r.required) {
                (Verdict::Fail, true) => "error",
                (Verdict::Fail, false) | (Verdict::Warn, _) => "warn",
                (Verdict::Pass, _) => continue,
            };
            let id = format!(
                "CONTRACT-{}",
                match r.group {
                    "document" => "DOC",
                    "component" => "COMP",
                    "interaction" => "INT",
                    "layout" => "LAY",
                    "behavior" => "BEH",
                    other => other.get(..3).unwrap_or("OTH"),
                }
            );
            out.push(Finding {
                id: id.to_string(),
                rule_id: None,
                severity: severity.into(),
                category: format!("contract/{}", r.group),
                summary: format!("[{}] {}: {}", r.verdict.as_str(), r.name, r.detail),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    format!("contract/{}", self.contract),
                    r.detail.clone(),
                )
                .with_detail(json!({
                    "contract": self.contract,
                    "group": r.group,
                    "check": r.name,
                    "verdict": r.verdict.as_str(),
                    "required": r.required,
                }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
        out
    }
}

/// Which behavior properties were actually observed by driving the app.
/// Filled by [`check_behavior`]; active oracles read from it instead of
/// guessing.
#[derive(Debug, Default, Clone)]
pub struct ObservedBehavior {
    pub escape_closes_modal: Option<bool>,
    pub reverse_tab_is_inverse: Option<bool>,
}

/// Run the full conformance check against a live session.
pub fn check_contract(
    session: &mut Session,
    contract: &ProjectContract,
) -> anyhow::Result<ContractReport> {
    let mut results = Vec::new();
    let mut driven = 0u32;

    // 1. Document validation (static).
    results.extend(validate_document(contract));

    // Restore point: audits must leave the terminal as they found it.
    let (orig_cols, orig_rows) = (session.cols(), session.rows());

    // 2. Components (static, current frame).
    results.extend(check_components(session, contract));

    // 3. Interactions (drives the app).
    results.extend(check_interactions(session, contract, &mut driven));

    // 4. Layout (drives resizes; restores).
    results.extend(check_layout(session, contract));

    // 5. Behavior properties (drives Escape / Tab probes) + top-level oracles.
    let observed = check_behavior(session, contract, &mut results, &mut driven);
    results.extend(check_top_level_oracles(session, contract, &observed));

    // Restore the viewport we came in with.
    // Canonical transaction (re-review P1 item 22): the restore is evidence,
    // not a side effect — same anchor/ledger/settle semantics as MCP acts.
    let _ = execute_act(
        session,
        &CanonicalAction::Resize { cols: orig_cols, rows: orig_rows },
        120,
        1500,
        false,
    );

    let verdict = results.iter().fold(Verdict::Pass, |acc, r| {
        let v = if r.verdict == Verdict::Fail && !r.required {
            // A failed optional check is a WARN overall, not a FAIL.
            Verdict::Warn
        } else {
            r.verdict
        };
        acc.merge(v)
    });

    Ok(ContractReport {
        contract: contract.schema.name.clone(),
        version: contract.schema.version.clone(),
        verdict,
        results,
        driven_actions: driven,
    })
}

/// Static document validation: oracles parse, regexes compile, required
/// names are unique, viewports are non-degenerate.
pub fn validate_document(c: &ProjectContract) -> Vec<CheckResult> {
    let mut out = Vec::new();

    // Every oracle expression anywhere in the document must parse.
    let mut exprs: Vec<(String, &String)> = Vec::new();
    for o in &c.oracles {
        let id = o.id.clone().unwrap_or_else(|| o.expr.clone());
        exprs.push((format!("oracles[{id}]"), &o.expr));
    }
    for comp in &c.components {
        for e in &comp.expect {
            exprs.push((format!("components[{}]", comp.name), e));
        }
    }
    for inter in &c.interactions {
        for e in &inter.expect {
            exprs.push((format!("interactions[{}]", inter.name), e));
        }
    }
    for (where_, expr) in exprs {
        match oracle::parse(expr) {
            Ok(o) => {
                if !oracle::KNOWN_PREDICATES.contains(&o.name.as_str()) {
                    out.push(CheckResult::fail(
                        "document",
                        where_.clone(),
                        format!(
                            "unknown oracle predicate '{}' in '{expr}' (known: {})",
                            o.name,
                            oracle::KNOWN_PREDICATES.join(", ")
                        ),
                    ));
                }
            }
            Err(e) => out.push(CheckResult::fail("document", where_, e.to_string())),
        }
    }

    // Volatile patterns must compile (they feed the normalization policy).
    for pat in &c.volatile_patterns {
        if let Err(e) = regex::Regex::new(pat) {
            out.push(CheckResult::fail(
                "document",
                format!("volatile_patterns[{pat}]"),
                format!("invalid regex: {e}"),
            ));
        }
    }

    // Unique names: duplicate component/interaction names make evidence
    // ambiguous.
    let dup = |names: Vec<String>, what: &str, out: &mut Vec<CheckResult>| {
        let mut seen = std::collections::HashSet::new();
        for n in names {
            if !seen.insert(n.clone()) {
                out.push(CheckResult::fail(
                    "document",
                    format!("{what} name uniqueness"),
                    format!("duplicate {what} name '{n}'"),
                ));
            }
        }
    };
    dup(
        c.components.iter().map(|x| x.name.clone()).collect(),
        "component",
        &mut out,
    );
    dup(
        c.interactions.iter().map(|x| x.name.clone()).collect(),
        "interaction",
        &mut out,
    );

    // Viewports must be non-degenerate.
    for v in &c.viewports {
        if v.cols == 0 || v.rows == 0 {
            out.push(CheckResult::fail(
                "document",
                "viewports",
                format!("degenerate viewport {}x{}", v.cols, v.rows),
            ));
        }
    }

    if out.is_empty() {
        out.push(CheckResult::pass(
            "document",
            "schema",
            format!(
                "'{}' v{}: all oracle expressions parse, patterns compile, names unique",
                c.schema.name, c.schema.version
            ),
        ));
    }
    out
}

// ── Components ──────────────────────────────────────────────────────────

fn check_components(session: &mut Session, contract: &ProjectContract) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if contract.components.is_empty() {
        return out;
    }
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            out.push(CheckResult::fail(
                "component",
                "observe",
                format!("cannot observe session: {e}"),
            ));
            return out;
        }
    };
    let sem = semantic::analyze(&screen);
    let tree = semantic::build_tree(&screen);

    for comp in &contract.components {
        out.push(check_one_component(&screen, &sem, &tree, comp));
    }
    out
}

fn check_one_component(
    screen: &crate::screen::ScreenState,
    sem: &semantic::SemanticScreen,
    tree: &semantic::SemanticTree,
    comp: &ComponentContract,
) -> CheckResult {
    let role = comp.role.trim().to_lowercase();
    let want = role.as_str();

    // 1) Screen-level components (table / tree / scrollbar).
    let components = semantic::detect_components(screen);
    let component_hit = components.iter().any(|c| match c {
        semantic::Component::Table(_) => want == "table",
        semantic::Component::Tree(_) => want == "tree",
        semantic::Component::Scrollbar(_) => want == "scrollbar",
    });

    // 2) Region kinds (dialog / panel / toolbar / footer / list …).
    let region_hit = sem
        .regions
        .iter()
        .any(|r| format!("{:?}", r.kind).to_lowercase() == want);

    // 3) Semantic node roles (menu, command_palette, …) — anywhere in the tree.
    let node_hit = role_matches(want, &tree.root);

    let found = component_hit || region_hit || node_hit;
    if !found {
        let result = CheckResult::fail(
            "component",
            comp.name.clone(),
            format!("component role '{}' not found on screen", comp.role),
        )
        .required(comp.required);
        return if comp.required {
            result
        } else {
            CheckResult {
                verdict: Verdict::Warn,
                ..result
            }
        };
    }

    // Found: evaluate the component's expect oracles against this frame.
    let mut sub = Vec::new();
    for e in &comp.expect {
        let outcome = oracle::eval_static(e, screen, sem);
        sub.push(to_sub_result("component", &comp.name, &outcome));
    }
    let failed = sub.iter().any(|r| r.verdict == Verdict::Fail);
    if sub.is_empty() {
        CheckResult::pass(
            "component",
            comp.name.clone(),
            format!("component role '{}' found", comp.role),
        )
        .required(comp.required)
    } else if failed {
        let detail = sub
            .iter()
            .filter(|r| r.verdict == Verdict::Fail)
            .map(|r| r.detail.clone())
            .collect::<Vec<_>>()
            .join("; ");
        CheckResult::fail("component", comp.name.clone(), detail).required(comp.required)
    } else {
        CheckResult::pass(
            "component",
            comp.name.clone(),
            format!(
                "component role '{}' found; {} expect oracle(s) passed",
                comp.role,
                sub.len()
            ),
        )
        .required(comp.required)
    }
}

/// Recursive role-slug match over the semantic node tree.
fn role_matches(want: &str, node: &semantic::SemanticNode) -> bool {
    if node.role.slug() == want {
        return true;
    }
    node.children.iter().any(|c| role_matches(want, c))
}

fn to_sub_result(group: &'static str, parent: &str, outcome: &OracleOutcome) -> CheckResult {
    let name = format!("{parent} :: {}", outcome.expr);
    if let Some(pe) = &outcome.parse_error {
        return CheckResult::fail(group, name, format!("oracle invalid: {pe}"));
    }
    if outcome.passed {
        CheckResult::pass(group, name, outcome.detail.clone())
    } else {
        CheckResult::fail(group, name, outcome.detail.clone())
    }
}

// ── Interactions ────────────────────────────────────────────────────────

fn check_interactions(
    session: &mut Session,
    contract: &ProjectContract,
    driven: &mut u32,
) -> Vec<CheckResult> {
    let mut out = Vec::new();
    for inter in &contract.interactions {
        out.push(run_interaction(session, inter, driven));
    }
    out
}

/// Send one interaction's keys through the canonical executor, then evaluate
/// its expect oracles against the final frame. A parse error in an oracle
/// fails the interaction but stays distinguishable (document group already
/// flagged it; the detail repeats it).
fn run_interaction(
    session: &mut Session,
    inter: &InteractionContract,
    driven: &mut u32,
) -> CheckResult {
    // Snapshot the starting structure so we can report whether the keys
    // actually changed the screen.
    let before_hash = match session.observe(40) {
        Ok(s) => s.structure_hash,
        Err(e) => {
            return CheckResult::fail(
                "interaction",
                inter.name.clone(),
                format!("cannot observe session: {e}"),
            )
        }
    };

    for key in &inter.keys {
        let req = serde_json::json!({ "action": "key", "key": key });
        let req = match serde_json::from_value::<crate::mcp::params::TuiActRequest>(req) {
            Ok(r) => r,
            Err(e) => {
                return CheckResult::fail(
                    "interaction",
                    inter.name.clone(),
                    format!("invalid key name '{key}': {e}"),
                )
            }
        };
        let action = match CanonicalAction::from_request(&req) {
            Ok(a) => a,
            Err(e) => {
                return CheckResult::fail(
                    "interaction",
                    inter.name.clone(),
                    format!("cannot build action for key '{key}': {e}"),
                )
            }
        };
        if let Err(e) = execute_act(session, &action, 80, 900, false) {
            return CheckResult::fail(
                "interaction",
                inter.name.clone(),
                format!("key '{key}' failed to execute: {e}"),
            );
        }
        *driven += 1;
    }

    // Evaluate the expect oracles against the settled frame.
    let screen = match session.observe(60) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult::fail(
                "interaction",
                inter.name.clone(),
                format!("cannot observe after keys: {e}"),
            )
        }
    };
    let sem = semantic::analyze(&screen);
    let changed = screen.structure_hash != before_hash;

    if inter.expect.is_empty() {
        return CheckResult::warn(
            "interaction",
            inter.name.clone(),
            format!(
                "keys sent ({}), screen changed: {changed}; no expect oracles declared — nothing proven",
                inter.keys.join(",")
            ),
        );
    }

    let mut failures = Vec::new();
    let mut passes = 0;
    for e in &inter.expect {
        let outcome = oracle::eval_static(e, &screen, &sem);
        if outcome.parse_error.is_some() || !outcome.passed {
            failures.push(outcome.detail.clone());
        } else {
            passes += 1;
        }
    }
    if failures.is_empty() {
        CheckResult::pass(
            "interaction",
            inter.name.clone(),
            format!(
                "keys [{}] → {} expect oracle(s) passed (screen changed: {changed})",
                inter.keys.join(","),
                passes
            ),
        )
    } else {
        CheckResult::fail(
            "interaction",
            inter.name.clone(),
            format!(
                "keys [{}] → {} of {} expect oracle(s) failed: {}",
                inter.keys.join(","),
                failures.len(),
                inter.expect.len(),
                failures.join("; ")
            ),
        )
    }
}

// ── Layout ──────────────────────────────────────────────────────────────

fn check_layout(session: &mut Session, contract: &ProjectContract) -> Vec<CheckResult> {
    let mut out = Vec::new();
    let (orig_cols, orig_rows) = (session.cols(), session.rows());

    // Declared viewports: resize + no clipping.
    for v in &contract.viewports {
        out.push(check_viewport(
            session,
            v.cols,
            v.rows,
            format!("viewport {}x{}", v.cols, v.rows),
        ));
    }
    // Layout constraints: minimum sizes + clipping requirement.
    for lc in &contract.layout {
        let name = lc.name.clone().unwrap_or_else(|| {
            format!(
                "layout {}x{}",
                lc.min_cols
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into()),
                lc.min_rows
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "?".into())
            )
        });
        if let (Some(cols), Some(rows)) = (lc.min_cols, lc.min_rows) {
            out.push(check_viewport(session, cols, rows, name));
        } else if lc.no_clipping {
            // No explicit size: check clipping at the current size.
            out.push(check_clipping_here(session, name, lc));
        }
    }

    // Canonical transaction (re-review P1 item 22): the restore is evidence,
    // not a side effect — same anchor/ledger/settle semantics as MCP acts.
    let _ = execute_act(
        session,
        &CanonicalAction::Resize { cols: orig_cols, rows: orig_rows },
        120,
        1500,
        false,
    );
    out
}

fn check_viewport(session: &mut Session, cols: u16, rows: u16, name: String) -> CheckResult {
    // Canonical transaction (re-review P1 item 22): the resize carries the
    // settle semantics and lands in the run ledger like any other action.
    // Item 23: the transaction's after-frame IS the authoritative
    // post-resize frame — no extra observe().
    let tx = match execute_act(
        session,
        &CanonicalAction::Resize { cols, rows },
        150,
        2000,
        false,
    ) {
        Ok(t) => t,
        Err(_) => {
            return CheckResult::fail(
                "layout",
                name,
                format!("resize to {cols}x{rows} failed"),
            )
        }
    };
    let screen = tx.after().clone();
    check_clipping_on_screen(
        &screen,
        name,
        &LayoutConstraint {
            name: None,
            min_cols: Some(cols),
            min_rows: Some(rows),
            no_clipping: true,
        },
    )
}

fn check_clipping_on_screen(
    screen: &crate::screen::ScreenState,
    name: String,
    lc: &LayoutConstraint,
) -> CheckResult {
    if !lc.no_clipping {
        return CheckResult::pass(
            "layout",
            name,
            "clipping not required to be checked".to_string(),
        );
    }
    let sem = semantic::analyze(screen);
    let clipped: Vec<String> = sem
        .regions
        .iter()
        .filter(|rg| !matches!(rg.clipping_state, semantic::ClippingState::None))
        .map(|rg| rg.id.clone())
        .collect();
    if clipped.is_empty() {
        CheckResult::pass("layout", name, "no region extends beyond the viewport".to_string())
    } else {
        CheckResult::fail(
            "layout",
            name,
            format!("clipped regions: {}", clipped.join(", ")),
        )
    }
}

fn check_clipping_here(session: &mut Session, name: String, lc: &LayoutConstraint) -> CheckResult {
    let screen = match session.observe(60) {
        Ok(s) => s,
        Err(e) => return CheckResult::fail("layout", name, format!("observe failed: {e}")),
    };
    if !lc.no_clipping {
        return CheckResult::pass(
            "layout",
            name,
            "clipping not required to be checked".to_string(),
        );
    }
    let sem = semantic::analyze(&screen);
    let clipped: Vec<String> = sem
        .regions
        .iter()
        .filter(|rg| !matches!(rg.clipping_state, semantic::ClippingState::None))
        .map(|rg| rg.id.clone())
        .collect();
    let overflow: Vec<String> = sem
        .regions
        .iter()
        .filter(|rg| {
            rg.bounds.x.saturating_add(rg.bounds.width) > screen.cols
                || rg.bounds.y.saturating_add(rg.bounds.height) > screen.rows
        })
        .map(|rg| rg.id.clone())
        .collect();
    if clipped.is_empty() && overflow.is_empty() {
        CheckResult::pass(
            "layout",
            name,
            format!(
                "no clipping at {}x{} ({} regions)",
                screen.cols,
                screen.rows,
                sem.regions.len()
            ),
        )
    } else {
        CheckResult::fail(
            "layout",
            name,
            format!(
                "clipping at {}x{}: clipped {clipped:?}, overflowing {overflow:?}",
                screen.cols, screen.rows
            ),
        )
    }
}

// ── Behavior (active properties) ────────────────────────────────────────

/// Prove the flag-backed properties by driving the app. Each property is
/// only checked when its precondition holds (no modal → Escape property is
/// vacuous and reported as such).
fn check_behavior(
    session: &mut Session,
    contract: &ProjectContract,
    results: &mut Vec<CheckResult>,
    driven: &mut u32,
) -> ObservedBehavior {
    let mut observed = ObservedBehavior::default();

    // ── escape_closes_modal ──
    if contract.escape_closes_modal {
        let pre = session.observe(50).ok();
        let modal_before = pre.as_ref().map(modal_present).unwrap_or(false);
        if !modal_before {
            // Establish the precondition from the contract itself: an
            // interaction whose expect declares `modal_open()` IS the
            // contract's modal-opener — run it so the property can be
            // proven rather than skipped.
            let opener = contract
                .interactions
                .iter()
                .find(|i| i.expect.iter().any(|e| e.trim().starts_with("modal_open")));
            let mut opened = false;
            if let Some(opener) = opener {
                for key in &opener.keys {
                    let req = serde_json::json!({"action": "key", "key": key});
                    let action = serde_json::from_value::<crate::mcp::params::TuiActRequest>(req)
                        .ok()
                        .and_then(|r| CanonicalAction::from_request(&r).ok());
                    if let Some(r) = action {
                        if execute_act(session, &r, 80, 900, false).is_ok() {
                            *driven += 1;
                            opened = true;
                        }
                    }
                }
                opened = opened
                    && session
                        .observe(60)
                        .map(|s| modal_present(&s))
                        .unwrap_or(false);
            }
            if !opened {
                results.push(CheckResult::warn(
                    "behavior",
                    "escape_closes_modal",
                    "skipped: no modal is open and no contract interaction declares modal_open() to establish one",
                ));
            } else {
                results.push(CheckResult::pass(
                    "behavior",
                    "escape_closes_modal",
                    format!(
                        "precondition established via interaction '{}' (modal observed open)",
                        opener.map(|o| o.name.as_str()).unwrap_or("?")
                    ),
                ));
            }
            // Only escape-check when a modal is actually up now.
            let modal_now = session
                .observe(50)
                .map(|s| modal_present(&s))
                .unwrap_or(false);
            if modal_now {
                let closed_ok = drive_escape_check(session);
                observed.escape_closes_modal = Some(closed_ok);
                *driven += 1;
                results.push(if closed_ok {
                    CheckResult::pass(
                        "behavior",
                        "escape_closes_modal",
                        "Escape dismissed the open modal (observed)",
                    )
                } else {
                    CheckResult::fail(
                        "behavior",
                        "escape_closes_modal",
                        "Escape did not dismiss the open modal (modal still present after Escape)",
                    )
                });
            }
        } else {
            let before_hash = pre.map(|s| s.structure_hash).unwrap_or_default();
            let req = serde_json::json!({"action": "key", "key": "escape"});
            let okc = serde_json::from_value::<crate::mcp::params::TuiActRequest>(req)
                .ok()
                .and_then(|r| CanonicalAction::from_request(&r).ok());
            if let Some(action) = okc {
                if execute_act(session, &action, 80, 900, false).is_ok() {
                    *driven += 1;
                    let after = session.observe(60).ok();
                    let modal_after = after.as_ref().map(modal_present).unwrap_or(true);
                    let changed = after
                        .as_ref()
                        .map(|s| s.structure_hash != before_hash)
                        .unwrap_or(false);
                    // The modal layer must actually be gone; the hash change
                    // is corroboration, not the verdict.
                    let closed = !modal_after && changed;
                    observed.escape_closes_modal = Some(closed);
                    results.push(if closed {
                        CheckResult::pass(
                            "behavior",
                            "escape_closes_modal",
                            "Escape dismissed the open modal (observed)",
                        )
                    } else {
                        CheckResult::fail(
                            "behavior",
                            "escape_closes_modal",
                            "Escape did not dismiss the open modal (modal still present, no screen change)",
                        )
                    });
                } else {
                    results.push(CheckResult::fail(
                        "behavior",
                        "escape_closes_modal",
                        "could not send Escape to the session",
                    ));
                }
            }
        }
    }

    // ── reverse_tab_required: prove via a mini focus graph ──
    if contract.reverse_tab_required {
        let mut graph = crate::semantic::focus_graph::FocusGraph::new();
        // Forward Tab sweep (bounded), then reverse Shift+Tab sweep.
        let mut forward = 0u32;
        for _ in 0..12 {
            let before = match session.observe(30) {
                Ok(s) => s,
                Err(_) => break,
            };
            let sem_b = semantic::analyze(&before);
            let req = serde_json::json!({"action": "key", "key": "tab"});
            let Some(action) = serde_json::from_value::<crate::mcp::params::TuiActRequest>(req)
                .ok()
                .and_then(|r| CanonicalAction::from_request(&r).ok())
            else {
                break;
            };
            let Ok(tx) = execute_act(session, &action, 80, 600, false) else {
                break;
            };
            *driven += 1;
            forward += 1;
            let after = tx.after().clone();
            let sem_a = semantic::analyze(&after);
            if let (Some(f), Some(t)) = (&sem_b.focus.control_id, &sem_a.focus.control_id) {
                graph.record_edge(f, t, "tab", sem_a.focus.control.as_deref());
            }
            if forward > 0 && sem_b.focus.control_id == sem_a.focus.control_id {
                // Focus stopped moving: cycle closed.
                break;
            }
        }
        let mut reverse_ok = true;
        for _ in 0..forward {
            let before = match session.observe(30) {
                Ok(s) => s,
                Err(_) => break,
            };
            let sem_b = semantic::analyze(&before);
            let req = serde_json::json!({"action": "key", "key": "shift+tab"});
            let Some(action) = serde_json::from_value::<crate::mcp::params::TuiActRequest>(req)
                .ok()
                .and_then(|r| CanonicalAction::from_request(&r).ok())
            else {
                reverse_ok = false;
                break;
            };
            let Ok(tx) = execute_act(session, &action, 80, 600, false) else {
                reverse_ok = false;
                break;
            };
            *driven += 1;
            let after = tx.after().clone();
            let sem_a = semantic::analyze(&after);
            if let (Some(f), Some(t)) = (&sem_b.focus.control_id, &sem_a.focus.control_id) {
                graph.record_edge(f, t, "shift+tab", sem_a.focus.control.as_deref());
            }
        }
        let gaps = graph.reverse_tab_gaps();
        if forward == 0 {
            results.push(CheckResult::warn(
                "behavior",
                "reverse_tab_required",
                "skipped: no Tab traversal possible on this screen",
            ));
        } else if !reverse_ok {
            results.push(CheckResult::warn(
                "behavior",
                "reverse_tab_required",
                "incomplete: Shift+Tab sweep could not complete; no verdict",
            ));
        } else if gaps.is_empty() {
            observed.reverse_tab_is_inverse = Some(true);
            results.push(CheckResult::pass(
                "behavior",
                "reverse_tab_required",
                format!("Shift+Tab exactly reversed Tab across {forward} forward step(s) (edge-for-edge)"),
            ));
        } else {
            observed.reverse_tab_is_inverse = Some(false);
            results.push(CheckResult::fail(
                "behavior",
                "reverse_tab_required",
                format!(
                    "Shift+Tab does not reverse Tab: {} edge(s) lack the inverse: {:?}",
                    gaps.len(),
                    gaps
                ),
            ));
        }
    }

    observed
}

fn modal_present(screen: &crate::screen::ScreenState) -> bool {
    let tree = crate::semantic::build_tree(screen);
    !tree
        .layer_nodes(crate::semantic::node::Layer::Modal)
        .is_empty()
}

/// Send Escape and report whether the modal layer is gone afterwards.
fn drive_escape_check(session: &mut Session) -> bool {
    let req = serde_json::json!({"action": "key", "key": "escape"});
    let Some(action) = serde_json::from_value::<crate::mcp::params::TuiActRequest>(req)
        .ok()
        .and_then(|r| CanonicalAction::from_request(&r).ok())
    else {
        return false;
    };
    if execute_act(session, &action, 80, 900, false).is_err() {
        return false;
    }
    session
        .observe(60)
        .map(|s| !modal_present(&s))
        .unwrap_or(false)
}

/// Top-level `oracles` — static ones evaluated against the current frame;
/// active ones against the behavior evidence collected above (or honestly
/// failed when that evidence is absent).
fn check_top_level_oracles(
    session: &mut Session,
    contract: &ProjectContract,
    observed: &ObservedBehavior,
) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if contract.oracles.is_empty() {
        return out;
    }
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            out.push(CheckResult::fail(
                "behavior",
                "oracles",
                format!("cannot observe session: {e}"),
            ));
            return out;
        }
    };
    let sem = semantic::analyze(&screen);
    let answers = ActiveArgs {
        escape_closes_modal: observed.escape_closes_modal,
        reverse_tab_is_inverse: observed.reverse_tab_is_inverse,
        ..ActiveArgs::none()
    };
    for o in &contract.oracles {
        let id = o.id.clone().unwrap_or_else(|| o.expr.clone());
        let outcome = if oracle::is_static(
            oracle::parse(&o.expr)
                .map(|p| p.name)
                .as_deref()
                .unwrap_or(""),
        ) {
            oracle::eval_static(&o.expr, &screen, &sem)
        } else {
            oracle::eval_active(&o.expr, &answers, &screen, &sem)
        };
        let mut r = to_sub_result("behavior", "oracle", &outcome);
        r.name = id;
        out.push(r);
    }
    out
}

/// Role slug for a [`Role`] — used by component contracts that declare a
/// node role (kept `pub` for the loader/docs).
pub fn role_slug(r: Role) -> String {
    r.slug()
}
