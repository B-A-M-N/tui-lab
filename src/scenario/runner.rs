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
                        let (step_passed, detail) =
                            match crate::mcp::helpers::build_input_from_request(&req) {
                                Ok(input) => {
                                    match crate::execution::execute_act(
                                        session,
                                        req.action_name(),
                                        input,
                                        150,
                                        1150,
                                        req.no_wait(),
                                    ) {
                                        Ok(tx) => (
                                            tx.settled,
                                            format!(
                                                "act executed, settled={} ({})",
                                                tx.settled,
                                                tx.settle_reason.unwrap_or_default()
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
                            None => (false, format!("unknown wait condition '{}'", wp.condition)),
                        },
                        Err(e) => (false, format!("unparseable wait step: {e}")),
                    }
                }
                StepKind::Assert => {
                    match session.observe(50) {
                        Ok(screen) => {
                            match serde_json::from_value::<TuiAssertParams>(step.params.clone()) {
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
