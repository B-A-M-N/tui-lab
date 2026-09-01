//! Scenario runner (spec item 39).
//!
//! Replays a recorded scenario against a session through the one canonical
//! executor (`crate::execution`) — audit re-review item 4: a scenario run
//! must actually send the inputs, run the waits, and evaluate the
//! assertions. Fabricated successes ("act step executed") and fabricated
//! screens (an 80x24 blank on observe failure) are gone: execution errors
//! fail the step with the real error.

use super::model::{ParameterValue, Scenario, StepKind};
use crate::mcp::params::{TuiActRequest, TuiAssertParams, TuiWaitParams};

#[derive(Debug, serde::Serialize)]
pub struct StepResult {
    pub index: usize,
    pub kind: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ScenarioRunReport {
    pub scenario_name: String,
    pub steps_total: usize,
    pub steps_passed: usize,
    pub steps_failed: usize,
    pub step_results: Vec<StepResult>,
}

/// Replays a scenario against a session.
pub struct ScenarioRunner;

impl ScenarioRunner {
    /// Run a scenario. Returns a report.
    ///
    /// Sensitive parameters (re-review P0.3) resolve from `values`; pass
    /// `&[]` when the scenario declares none. Equivalent to
    /// [`ScenarioRunner::run`] with no parameter values.
    pub fn run(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
    ) -> ScenarioRunReport {
        Self::run_with_parameters(scenario, session, &[])
    }

    /// Run a scenario with caller-supplied sensitive-parameter values
    /// (re-review P0.3). Values substitute `${NAME}` references in step
    /// payloads; a reference whose value was not supplied fails its step as
    /// `unresolved_parameter` — structured and honest — rather than
    /// deserializing to a parse error or, worse, sending a literal
    /// `${PASSWORD}` keystroke into the app.
    pub fn run_with_parameters(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
    ) -> ScenarioRunReport {
        let mut results = Vec::new();
        let mut passed = 0;
        let mut failed = 0;

        // Resolve the whole step list up front so ${NAME} references become
        // real payloads before any step executes. Unresolved references are
        // left in place and detected per-step below.
        let resolved_params = scenario.resolve_parameters(values);

        for (i, step) in scenario.steps.iter().enumerate() {
            let params = resolved_params[i].clone();
            // Sensitive-parameter resolution check (re-review P0.3): before
            // anything executes, a step still carrying a ${NAME} reference
            // the scenario declares means the caller omitted the value.
            if let Some(missing) = unresolved_reference(&params, scenario) {
                results.push(StepResult {
                    index: i,
                    kind: format!("{:?}", step.kind).to_lowercase(),
                    passed: false,
                    detail: format!(
                        "unresolved_parameter: '{}' was not supplied (declare it via \
                         scenario parameters and pass its value at replay time)",
                        missing
                    ),
                });
                failed += 1;
                continue;
            }
            // Mutation guard (re-review Wave-2): a recorded precondition is
            // verified against the LIVE screen before the step's input
            // lands. A drift verdict fails the step without touching the
            // app — the alternative (send into the wrong UI) corrupts both.
            if let Some(expect) = &step.expect {
                let (guard_ok, guard_detail) = check_expect(session, expect);
                if !guard_ok {
                    results.push(StepResult {
                        index: i,
                        kind: format!("{:?}", step.kind),
                        passed: false,
                        detail: format!("stale_state: {guard_detail}"),
                    });
                    failed += 1;
                    continue;
                }
            }
            let (step_passed, detail) = match step.kind {
                StepKind::Act => match serde_json::from_value::<TuiActRequest>(params) {
                    Ok(req) => {
                        // Typed action (Wave-2 item 10): the scenario step's
                        // JSON is the same shape the live MCP call carried,
                        // so replay executes exactly what was recorded.
                        //
                        // Re-review P0.2: replay honors the RECORDED
                        // completion and quiet window. The old path forced
                        // StableScreen/150/1150, so a recorded
                        // `key q + completion=process_exit` replayed as a
                        // 150ms screen settle — different semantics, false
                        // failures on silent/exit actions, and a scenario
                        // that was never an exact recording of live
                        // behavior.
                        let vis = if req.sensitive() {
                            crate::execution::InputVisibility::Sensitive
                        } else {
                            crate::execution::InputVisibility::Normal
                        };
                        let quiet = req
                            .wait_ms()
                            .or_else(|| req.completion_quiet_ms())
                            .unwrap_or(150);
                        let completion =
                            req.completion()
                                .unwrap_or(crate::capture::CompletionPolicy::StableScreen);
                        let (step_passed, detail) =
                            match crate::execution::CanonicalAction::from_request(&req) {
                                Ok(action) => {
                                    match crate::execution::execute_act_with_completion(
                                        session,
                                        &action,
                                        quiet,
                                        quiet.saturating_add(1000),
                                        req.no_wait(),
                                        vis,
                                        completion,
                                    ) {
                                        Ok(tx) => (
                                            tx.settled(),
                                            format!(
                                                "act executed, settled={:?} ({})",
                                                tx.settle,
                                                tx.settle_reason()
                                            ),
                                        ),
                                        Err(e) => (false, format!("act failed: {e}")),
                                    }
                                }
                                Err(msg) => (false, format!("invalid act params: {msg}")),
                            };
                        (step_passed, detail)
                    }
                    Err(e) => (false, format!("unparseable act step: {e}")),
                },
                StepKind::Wait => {
                    match serde_json::from_value::<TuiWaitParams>(params.clone()) {
                        Ok(wp) => match crate::mcp::helpers::build_wait(&wp) {
                            Some(cond) => {
                                match crate::execution::execute_wait(
                                    session,
                                    cond,
                                    wp.budget_ms.unwrap_or(5000),
                                ) {
                                    Ok(out) => (
                                        out.met,
                                        format!("wait met={} reason={:?}", out.met, out.reason),
                                    ),
                                    Err(e) => (false, format!("wait failed: {e}")),
                                }
                            }
                            None => (
                                false,
                                format!(
                                    "unknown wait condition '{}' (expected one of: {})",
                                    match &wp.condition {
                                        crate::mcp::params::Known::Other(o) => o.clone(),
                                        _ => String::new(),
                                    },
                                    <crate::mcp::params::WaitCondition as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                                ),
                            ),
                        },
                        Err(e) => (false, format!("unparseable wait step: {e}")),
                    }
                }
                StepKind::Assert => {
                    match session.observe(50) {
                        Ok(screen) => {
                            // Wave E: an oracle assert step carries a
                            // declarative oracle expression ({"assertion":
                            // "oracle", "text": "modal_open()"}), evaluated
                            // through the same language contracts and audits
                            // use.
                            let is_oracle = params.get("assertion").and_then(|a| a.as_str())
                                == Some("oracle");
                            if is_oracle {
                                let expr =
                                    params.get("text").and_then(|t| t.as_str()).or_else(
                                        || params.get("reference").and_then(|t| t.as_str()),
                                    );
                                match expr {
                                    Some(expr) => {
                                        // Re-review P0.5: oracle evaluation reads
                                        // the FUSED semantic truth (native
                                        // channel participates), the same
                                        // analysis `tui_observe semantic` shows —
                                        // never a bare re-inference that can
                                        // disagree with the observation the
                                        // caller just made.
                                        session.poll_native();
                                        let sem = session.fuse_screen(&screen);
                                        let outcome =
                                            crate::design::eval_static(expr, &screen, &sem);
                                        if outcome.parse_error.is_some() {
                                            (false, format!("invalid oracle: {}", outcome.detail))
                                        } else if outcome.passed {
                                            (true, format!("oracle '{expr}': {}", outcome.detail))
                                        } else {
                                            (
                                                false,
                                                format!(
                                                    "oracle '{expr}' failed: {}",
                                                    outcome.detail
                                                ),
                                            )
                                        }
                                    }
                                    None => (
                                        false,
                                        "oracle step missing expression (expected 'text')".into(),
                                    ),
                                }
                            } else {
                                match serde_json::from_value::<TuiAssertParams>(params.clone()) {
                                    Ok(ap) => {
                                        let (p, d, invalid) =
                                            crate::execution::execute_assert(&ap, &screen);
                                        if invalid.is_some() {
                                            (false, format!("invalid assertion: {d}"))
                                        } else {
                                            (p, d)
                                        }
                                    }
                                    Err(e) => (false, format!("unparseable assert step: {e}")),
                                }
                            }
                        }
                        // A failed observation is an execution error, never a
                        // fabricated blank screen (re-review item 4).
                        Err(e) => (false, format!("observe failed during assert: {e}")),
                    }
                }
            };

            if step_passed {
                passed += 1;
            } else {
                failed += 1;
            }

            results.push(StepResult {
                index: i,
                kind: format!("{:?}", step.kind).to_lowercase(),
                passed: step_passed,
                detail,
            });
        }

        ScenarioRunReport {
            scenario_name: scenario.name.clone(),
            steps_total: scenario.steps.len(),
            steps_passed: passed,
            steps_failed: failed,
            step_results: results,
        }
    }
}

/// Verify one recorded precondition against the live session. The screen is
/// fused (native overlay participates) so a cooperative app's focus counts.
/// Every listed condition must hold; the first failure names itself.
fn check_expect(
    session: &mut crate::session::state::Session,
    expect: &super::model::StepExpect,
) -> (bool, String) {
    session.poll_native();
    let screen = match session.last().cloned() {
        Some(s) => s,
        None => match session.observe(0) {
            Ok(s) => s,
            Err(e) => return (false, format!("no frame to verify against: {e}")),
        },
    };
    let fused = session.fuse_screen(&screen);
    if let Some(want) = &expect.structure_hash {
        if &screen.structure_hash != want {
            return (
                false,
                format!(
                    "structure drifted since capture: expected {}, live {}",
                    want, screen.structure_hash
                ),
            );
        }
    }
    if let Some(want_focus) = &expect.focus_control_id {
        if fused.focus.control_id.as_deref() != Some(want_focus.as_str()) {
            return (
                false,
                format!(
                    "focus moved since capture: expected {want_focus}, live {:?}",
                    fused.focus.control_id
                ),
            );
        }
    }
    if let Some(text) = &expect.text_present {
        let present = screen
            .viewport_text
            .iter()
            .any(|r| r.contains(text.as_str()))
            || screen.scrollback.iter().any(|r| r.contains(text.as_str()));
        if !present {
            return (false, format!("required text absent: {text:?}"));
        }
    }
    (true, "preconditions hold".to_string())
}

/// The first declared-but-unsupplied `${NAME}` reference remaining in a
/// step's params (re-review P0.3), or `None` when the step is fully
/// resolved. Only DECLARED parameters count: an undeclared `${...}` in a
/// payload is the caller's literal text, not our business.
fn unresolved_reference(
    params: &serde_json::Value,
    scenario: &Scenario,
) -> Option<String> {
    for p in &scenario.parameters {
        let needle = p.reference();
        let mut found = false;
        walk_strings(params, &mut |s| {
            if s.contains(&needle) {
                found = true;
            }
        });
        if found {
            return Some(p.name.clone());
        }
    }
    None
}

/// Visit every string in a JSON value.
fn walk_strings(v: &serde_json::Value, f: &mut impl FnMut(&str)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(items) => items.iter().for_each(|i| walk_strings(i, f)),
        serde_json::Value::Object(map) => map.values().for_each(|i| walk_strings(i, f)),
        _ => {}
    }
}
