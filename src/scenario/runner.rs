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

/// How a replay ended (audit finding 5): a stop-policy replay that hit a
/// failure says `stopped_on_failure` — `completed` never appears next to a
/// skipped step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Every step ran to the end.
    Completed,
    /// A step failed under the stop policy; the rest were skipped.
    StoppedOnFailure,
}

#[derive(Debug, serde::Serialize)]
pub struct ScenarioRunReport {
    pub scenario_name: String,
    /// How the replay ended (see [`RunStatus`]).
    pub status: RunStatus,
    pub steps_total: usize,
    pub steps_passed: usize,
    pub steps_failed: usize,
    /// Steps NOT attempted because the stop policy halted the replay after
    /// an earlier failure (audit finding 5). Distinct from failed: a skipped
    /// step proves nothing either way.
    pub steps_skipped: usize,
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
        Self::run_in_run(scenario, session, &[], None)
    }

    /// Run a scenario inside a run context (audit P0-21): every executed
    /// act step lands in the transaction ledger / frame evidence through
    /// the shared [`crate::execution::drive`] pipeline, so replay leaves
    /// the same reconstructable evidence live acts do.
    pub fn run_in_run(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
        run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
    ) -> ScenarioRunReport {
        Self::run_inner(scenario, session, values, run, None)
    }

    /// [`Self::run_in_run`] with an explicit failure-policy override (audit
    /// finding 5): the caller (the MCP `run` action) may pass `on_failure`
    /// on the wire; `None` uses the scenario's own recorded policy.
    pub fn run_in_run_with_policy(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
        run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        policy: Option<super::model::FailurePolicy>,
    ) -> ScenarioRunReport {
        Self::run_inner(scenario, session, values, run, policy)
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
        Self::run_inner(scenario, session, values, None, None)
    }

    fn run_inner(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
        run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        policy_override: Option<super::model::FailurePolicy>,
    ) -> ScenarioRunReport {
        let mut results = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        let mut skipped = 0;
        // Effective policy: caller override wins over the recorded one; the
        // serde default for old scenario files is stop — continuing past a
        // failed step compounds the failure it should be reporting (audit
        // finding 5).
        let policy = policy_override.unwrap_or(scenario.on_failure);
        // Set when the stop policy halts the replay; every later step is
        // then recorded as skipped, and the report's status says stopped.
        let mut stopped_at: Option<usize> = None;

        // Resolve the whole step list up front so ${NAME} references become
        // real payloads before any step executes. Unresolved references are
        // left in place and detected per-step below.
        let resolved_params = scenario.resolve_parameters(values);

        for (i, step) in scenario.steps.iter().enumerate() {
            // Finding 5, fail-fast: once the stop policy triggered, the
            // remaining steps are NOT run and NOT counted as failures —
            // they are skipped, each with the reason and the step that
            // caused the halt.
            if let Some(at) = stopped_at {
                results.push(StepResult {
                    index: i,
                    kind: format!("{:?}", step.kind).to_lowercase(),
                    passed: false,
                    detail: format!(
                        "skipped_due_to_prior_failure: step {at} failed and the scenario's \
                         on_failure policy is 'stop'; this step was not attempted"
                    ),
                });
                skipped += 1;
                continue;
            }
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
                if policy == super::model::FailurePolicy::Stop {
                    stopped_at = Some(i);
                }
                continue;
            }
            // Mutation guard (re-review Wave-2 + audit P0-19): a recorded
            // precondition is verified against the LIVE screen BEFORE the
            // step's input lands. For Act steps it is compiled into the
            // executor's MutationGuard — validated atomically with the
            // send, closing the old TOCTOU window where the precondition
            // was checked from a possibly-seconds-old frame and the input
            // went out afterward. Wait/Assert steps send nothing, so the
            // direct check below has no window to close.
            let expect_guard: Option<crate::execution::MutationGuard> = match &step.expect {
                Some(expect) if step.kind == StepKind::Act => {
                    let (guard_ok, guard_detail, guard) = compile_expect_guard(session, expect);
                    if !guard_ok {
                        results.push(StepResult {
                            index: i,
                            kind: format!("{:?}", step.kind),
                            passed: false,
                            detail: format!("stale_state: {guard_detail}"),
                        });
                        failed += 1;
                        if policy == super::model::FailurePolicy::Stop {
                            stopped_at = Some(i);
                        }
                        continue;
                    }
                    guard
                }
                Some(expect) => {
                    let (guard_ok, guard_detail) = check_expect(session, expect);
                    if !guard_ok {
                        results.push(StepResult {
                            index: i,
                            kind: format!("{:?}", step.kind),
                            passed: false,
                            detail: format!("stale_state: {guard_detail}"),
                        });
                        failed += 1;
                        if policy == super::model::FailurePolicy::Stop {
                            stopped_at = Some(i);
                        }
                        continue;
                    }
                    None
                }
                None => None,
            };
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
                        let budget = req.settle_budget_ms().unwrap_or(quiet.saturating_add(1000));
                        let completion = req
                            .completion()
                            .unwrap_or(crate::capture::CompletionPolicy::StableScreen);
                        let (step_passed, detail) =
                            match crate::execution::CanonicalAction::from_request(&req) {
                                Ok(action) => {
                                    // Audit P0-21: when the replay runs inside a
                                    // run context, the act goes through the ONE
                                    // driving pipeline — frames, ledger
                                    // transaction, event/coverage fold — exactly
                                    // like a live tui_act. Scenario capture stays
                                    // off (the step IS the scenario); the
                                    // envelope-sensitive visibility still rides.
                                    let outcome = match run {
                                        Some(run) => {
                                            // Beta-audit P0-6: capture the run
                                            // identity at replay dispatch; the
                                            // pipeline verifies at commit.
                                            let ticket = crate::execution::RunTicket::capture(run);
                                            let spec = crate::execution::CoreDriveSpec {
                                                action: &action,
                                                quiet_ms: quiet,
                                                budget_ms: budget,
                                                no_wait: req.no_wait(),
                                                visibility: vis,
                                                completion,
                                                guard: expect_guard.as_ref(),
                                                scenario: None,
                                                origin: crate::execution::DriveOrigin::Scenario,
                                                ticket,
                                            };
                                            crate::execution::drive_pipeline(session, run, spec)
                                                .map(|o| o.tx)
                                        }
                                        None => {
                                            crate::execution::execute_act_with_guard_and_origin(
                                                session,
                                                crate::execution::DriveOrigin::Scenario,
                                                &action,
                                                quiet,
                                                budget,
                                                req.no_wait(),
                                                vis,
                                                completion,
                                                expect_guard.as_ref(),
                                            )
                                        }
                                    };
                                    match outcome {
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
                        Ok(wp) => {
                            // Audit finding 20: a wait step may be an EVENT
                            // wait (condition=event). Route it through the
                            // SAME shared primitive the live `tui_wait` uses
                            // — event history is a session-queue concern, not
                            // a backend read condition, and a recorded event
                            // wait must replay with identical semantics.
                            if matches!(
                                wp.condition.known(),
                                Some(crate::mcp::params::WaitCondition::Event)
                            ) {
                                match wp.event.clone() {
                                    Some(pred) => {
                                        let budget = wp.budget_ms.unwrap_or(5000);
                                        match crate::execution::execute_wait_event(
                                            session, &pred, budget,
                                        ) {
                                            Ok(out) => (
                                                out.met,
                                                format!(
                                                    "event wait met={} matched_seq={}",
                                                    out.met, out.matched_seq
                                                ),
                                            ),
                                            Err(e) => (false, format!("event wait failed: {e}")),
                                        }
                                    }
                                    None => (
                                        false,
                                        "event wait requires the 'event' predicate object"
                                            .to_string(),
                                    ),
                                }
                            } else {
                                match crate::mcp::helpers::build_wait(&wp) {
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
                                }
                            }
                        }
                        Err(e) => (false, format!("unparseable wait step: {e}")),
                    }
                }
                StepKind::Intent => {
                    // Beta-audit P0-9: a recorded semantic intent replays as
                    // an INTENT, not as frozen keys. The target is
                    // re-resolved against the LIVE screen and the same
                    // focus-secured plan engine runs (move_focus →
                    // assert_focus → act), so a recording survives layout
                    // and focus changes that would break a replayed Tab
                    // sequence. Sensitive `type` payloads keep their
                    // visibility policy.
                    match serde_json::from_value::<crate::intent::ActionTarget>(
                        params
                            .get("target")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    )
                    .map_err(|e| format!("unparseable intent target: {e}"))
                    .and_then(|target| {
                        serde_json::from_value::<crate::mcp::params::IntentVerbParam>(
                            params
                                .get("verb")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                        .map_err(|e| format!("unparseable intent verb: {e}"))
                        .and_then(|v| v.parse().map_err(|e| format!("invalid intent verb: {e}")))
                        .map(|verb| (target, verb))
                    }) {
                        Ok((target, verb)) => {
                            let sensitive = params
                                .get("sensitive")
                                .and_then(|s| s.as_bool())
                                .unwrap_or(false);
                            let vis = if sensitive {
                                crate::execution::InputVisibility::Sensitive
                            } else {
                                crate::execution::InputVisibility::Normal
                            };
                            // Fresh observation: the plan resolves against
                            // what the screen shows NOW (same audit-P0-13
                            // rule the live path follows).
                            let (step_passed, detail) = match session.observe_fused(40) {
                                Err(e) => (false, format!("pre-intent observe failed: {e}")),
                                Ok((_, sem, _, _)) => {
                                    // The proven FocusGraph — intent steps
                                    // replay under the SAME focus-route
                                    // provenance rule the live path enforces.
                                    let graph = match run {
                                        Some(run) => {
                                            run.lock().unwrap().graphs().focus_graph.clone()
                                        }
                                        None => crate::semantic::focus_graph::FocusGraph::new(),
                                    };
                                    match crate::intent::plan_intent_with_graph(
                                        &sem, &target, verb, &graph,
                                    ) {
                                        Err(e) => (
                                            false,
                                            format!("intent re-resolution failed: {}", e.message()),
                                        ),
                                        Ok(plan) => {
                                            let mut ok = true;
                                            let mut why = String::new();
                                            for step in &plan.steps {
                                                match step {
                                                    crate::intent::PlannedStep::MoveFocus {
                                                        key,
                                                        ..
                                                    } => {
                                                        let (
                                                            focus_completion,
                                                            focus_quiet,
                                                            focus_budget,
                                                            focus_no_wait,
                                                        ) = intent_execution_policy(&params);
                                                        if let Err(e) = drive_intent_step(
                                                            session,
                                                            run,
                                                            key,
                                                            vis,
                                                            focus_completion,
                                                            focus_quiet,
                                                            focus_budget,
                                                            focus_no_wait,
                                                        ) {
                                                            ok = false;
                                                            why = format!("focus move failed: {e}");
                                                            break;
                                                        }
                                                        // Refresh the session's
                                                        // last frame so the
                                                        // assert below reads the
                                                        // post-move screen (same
                                                        // rule as the live path).
                                                        if let Err(e) = session.observe(0) {
                                                            ok = false;
                                                            why = format!(
                                                                "post-focus observe failed: {e}"
                                                            );
                                                            break;
                                                        }
                                                    }
                                                    crate::intent::PlannedStep::AssertFocus {
                                                        target_id,
                                                    } => {
                                                        let live = session
                                                            .analyze_last()
                                                            .map(|a| a.semantic)
                                                            .and_then(|s| s.focus.control_id);
                                                        if live.as_deref()
                                                            != Some(target_id.as_str())
                                                        {
                                                            ok = false;
                                                            why = format!(
                                                                "focus assertion failed: expected {target_id}, focus holds {live:?}"
                                                            );
                                                            break;
                                                        }
                                                    }
                                                    crate::intent::PlannedStep::Act(action) => {
                                                        let (
                                                            payload_completion,
                                                            payload_quiet,
                                                            payload_budget,
                                                            payload_no_wait,
                                                        ) = intent_execution_policy(&params);
                                                        if let Err(e) = drive_intent_step(
                                                            session,
                                                            run,
                                                            action,
                                                            vis,
                                                            payload_completion,
                                                            payload_quiet,
                                                            payload_budget,
                                                            payload_no_wait,
                                                        ) {
                                                            ok = false;
                                                            why =
                                                                format!("payload act failed: {e}");
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                            if ok {
                                                (
                                                    true,
                                                    format!(
                                                        "intent replayed: {} step(s) focus-secured",
                                                        plan.steps.len()
                                                    ),
                                                )
                                            } else {
                                                (false, why)
                                            }
                                        }
                                    }
                                }
                            };
                            (step_passed, detail)
                        }
                        Err(msg) => (false, msg),
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
                            let is_oracle =
                                params.get("assertion").and_then(|a| a.as_str()) == Some("oracle");
                            if is_oracle {
                                let expr = params
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .or_else(|| params.get("reference").and_then(|t| t.as_str()));
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
                // Finding 5: the stop policy halts HERE — before the next
                // step is attempted. `Continue` runs on as before.
                if policy == super::model::FailurePolicy::Stop {
                    stopped_at = Some(i);
                }
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
            status: if stopped_at.is_some() {
                RunStatus::StoppedOnFailure
            } else {
                RunStatus::Completed
            },
            steps_total: scenario.steps.len(),
            steps_passed: passed,
            steps_failed: failed,
            steps_skipped: skipped,
            step_results: results,
        }
    }
}

/// Beta-audit P0-9: one focus-secured intent step at replay time — the
/// same driving pipeline the live intent path uses (frames, ledger,
/// event fold under the run's ticket), so a recorded intent's replay is
/// evidenced identically to its original execution.
#[allow(clippy::too_many_arguments)]
fn drive_intent_step(
    session: &mut crate::session::state::Session,
    run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
    action: &crate::execution::CanonicalAction,
    vis: crate::execution::InputVisibility,
    completion: crate::capture::CompletionPolicy,
    quiet_ms: u64,
    budget_ms: u64,
    no_wait: bool,
) -> Result<crate::execution::InteractionTransaction, anyhow::Error> {
    match run {
        Some(run) => {
            let ticket = crate::execution::RunTicket::capture(run);
            let spec = crate::execution::CoreDriveSpec {
                action,
                quiet_ms,
                budget_ms,
                no_wait,
                visibility: vis,
                completion,
                guard: None,
                scenario: None,
                origin: crate::execution::DriveOrigin::Scenario,
                ticket,
            };
            crate::execution::drive_pipeline(session, run, spec).map(|o| o.tx)
        }
        None => crate::execution::execute_act_with_completion(
            session, action, quiet_ms, budget_ms, no_wait, vis, completion,
        ),
    }
}

/// Decode the recorded intent execution policy. Older scenarios without
/// these fields keep the documented 150ms/1150ms StableScreen behavior.
fn intent_execution_policy(
    params: &serde_json::Value,
) -> (crate::capture::CompletionPolicy, u64, u64, bool) {
    use crate::capture::CompletionPolicy as CP;
    let quiet = params
        .get("quiet_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(150);
    let budget = params
        .get("settle_budget_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(quiet.saturating_add(1000));
    let no_wait = params
        .get("no_wait")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let completion = params
        .get("completion")
        .and_then(|v| v.as_str())
        .map(|name| match name {
            "first_change" => CP::FirstScreenChange,
            "any_change" => CP::AnyObservableChange,
            "text_appears" => CP::TextAppears(
                params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
            "text_disappears" => CP::TextDisappears(
                params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
            "process_exit" => CP::ProcessExit,
            "command_done" => CP::CommandDone,
            "bell" => CP::Bell,
            "semantic_change" => CP::SemanticChange,
            "may_be_silent" => CP::MayBeSilent,
            "no_wait" => CP::NoWait,
            _ => CP::StableScreen,
        })
        .unwrap_or(CP::StableScreen);
    (completion, quiet, budget, no_wait)
}

/// Compile a recorded precondition into an executor [`MutationGuard`]
/// (audit P0-19). Returns `(holds_now, detail, guard)`: `holds_now` is a
/// fast pre-check (a precondition already violated fails the step without
/// attempting the send), and `guard` carries the structural/focus
/// expectations into the executor for re-validation ATOMICALLY WITH THE
/// SEND — the screen cannot drift between check and input anymore.
/// `text_present` has no guard slot; it is checked here (pre-send) and
/// re-checked implicitly by the guard's structure hash when layout
/// tracks content, which is the honest best available.
fn compile_expect_guard(
    session: &mut crate::session::state::Session,
    expect: &super::model::StepExpect,
) -> (bool, String, Option<crate::execution::MutationGuard>) {
    let (ok, detail) = check_expect(session, expect);
    if !ok {
        return (false, detail, None);
    }
    let guard = crate::execution::MutationGuard {
        structure_hash: expect.structure_hash.clone(),
        focus_control_id: expect.focus_control_id.clone(),
        ..Default::default()
    };
    (true, detail, Some(guard))
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
fn unresolved_reference(params: &serde_json::Value, scenario: &Scenario) -> Option<String> {
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
