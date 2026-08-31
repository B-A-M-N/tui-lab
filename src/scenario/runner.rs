//! Scenario runner (spec item 39).
//!
//! Replays a recorded scenario against a session through the one canonical
//! executor (`crate::execution`) — audit re-review item 4: a scenario run
//! must actually send the inputs, run the waits, and evaluate the
//! assertions. Fabricated successes ("act step executed") and fabricated
//! screens (an 80x24 blank on observe failure) are gone: execution errors
//! fail the step with the real error.

use super::model::{Scenario, StepKind};
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
    pub fn run(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
    ) -> ScenarioRunReport {
        let mut results = Vec::new();
        let mut passed = 0;
        let mut failed = 0;

        for (i, step) in scenario.steps.iter().enumerate() {
            let (step_passed, detail) = match step.kind {
                StepKind::Act => match serde_json::from_value::<TuiActRequest>(step.params.clone())
                {
                    Ok(req) => {
                        // Typed action (Wave-2 item 10): the scenario step's
                        // JSON is the same shape the live MCP call carried,
                        // so replay executes exactly what was recorded.
                        // Sensitive steps route through the redacting executor
                        // so a replayed secret never lands in a cast either
                        // (leak fix).
                        let vis = if req.sensitive() {
                            crate::execution::InputVisibility::Sensitive
                        } else {
                            crate::execution::InputVisibility::Normal
                        };
                        let (step_passed, detail) =
                            match crate::execution::CanonicalAction::from_request(&req) {
                                Ok(action) => {
                                    match crate::execution::execute_act_with_visibility(
                                        session,
                                        &action,
                                        150,
                                        1150,
                                        req.no_wait(),
                                        vis,
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
                    match serde_json::from_value::<TuiWaitParams>(step.params.clone()) {
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
                            let is_oracle = step.params.get("assertion").and_then(|a| a.as_str())
                                == Some("oracle");
                            if is_oracle {
                                let expr =
                                    step.params.get("text").and_then(|t| t.as_str()).or_else(
                                        || step.params.get("reference").and_then(|t| t.as_str()),
                                    );
                                match expr {
                                    Some(expr) => {
                                        let sem = crate::semantic::analyze(&screen);
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
                                match serde_json::from_value::<TuiAssertParams>(step.params.clone())
                                {
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
