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

/// Typed step status (P1-22): consumers key on status, never prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped,
}

/// Stable error category for a failed/skipped step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepErrorCategory {
    UnresolvedParameter,
    StaleState,
    InvalidParams,
    ExecutionError,
    Timeout,
    /// An assertion predicate evaluated and did not hold (replay
    /// semantics, audit finding 22: distinct from a transport failure).
    AssertionFailed,
    SkippedDueToPriorFailure,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StepResult {
    pub index: usize,
    pub kind: String,
    pub passed: bool,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<StepErrorCategory>,
    pub detail: String,
    /// Executed act-transaction ledger sequence, when the replay ran inside
    /// a run and evidence committed successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioRunReport {
    pub scenario_name: String,
    pub scenario_id: String,
    pub scenario_schema: String,
    /// Monotonic process-start milliseconds at run start/end. These bound
    /// the causal run, unlike wall-clock metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_monotonic_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_monotonic_ms: Option<u64>,
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

/// Reset contract for repeated scenario execution (audit findings 18/24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    /// Stop, discard the old process, and relaunch the scenario-owned
    /// target before every repeat.
    Restart,
    /// Continue on the current mutable state. With `repeat > 1` this is
    /// an explicit request for continuous observations, not flakiness.
    Continue,
}

/// Aggregate verdict across repeated runs (P1-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakinessVerdict {
    /// Every repeat passed. With `repeat=1`, this is an ordinary pass.
    StablePass,
    /// Every repeat failed. With `repeat=1`, this is an ordinary failure.
    StableFailure,
    /// Mixed pass/fail outcomes across repeats.
    Flaky,
}

impl FlakinessVerdict {
    pub fn name(&self) -> &'static str {
        match self {
            Self::StablePass => "stable_pass",
            Self::StableFailure => "stable_failure",
            Self::Flaky => "flaky",
        }
    }
}

/// Aggregate result for repeated scenario replay. `runs` retains only the
/// first and last run when repeat > 2; complete histories would scale N×
/// exactly the way flakiness evidence should not.
#[derive(Debug, serde::Serialize)]
pub struct ScenarioRepeatReport {
    pub repeat: u32,
    pub passed_runs: u32,
    pub failed_runs: u32,
    pub verdict: FlakinessVerdict,
    /// Pass rate as a percentage (0–100), rounded down.
    pub pass_rate_pct: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_run: Option<ScenarioRunReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ScenarioRunReport>,
}

impl ScenarioRunner {
    /// Repeat a replay and classify flakiness (P1-23) with a RESET
    /// CONTRACT between iterations. A repeat count of 1 is equivalent to
    /// [`Self::run_in_run_with_policy`] plus an aggregate verdict.
    ///
    /// Audit finding 18: repeats against the SAME mutable session
    /// consecutively are state-contaminated — run 2 begins from run 1's
    /// final state, so a mixed pass/fail is deterministic accumulated UI
    /// state, not flakiness. `repeat > 1` therefore REQUIRES
    /// `reset_between`: when `true`, the session is restarted (same launch
    /// spec, new generation) before every run after the first; when
    /// `false`, the runner REFUSES to classify — returning a single
    /// `NoResetContract` verdict-style report (all steps skipped, no
    /// flakiness claim) rather than manufacturing contaminated repeats.
    /// Repeat a replay and classify flakiness. The supplied session must
    /// already have been launched from the scenario-owned target; before
    /// every iteration (including the first) the runner verifies the
    /// session's stored launch spec and advances generation through
    /// `restart()`. This keeps scenario-owned repeats isolated from prior
    /// UI state and prevents a generic inherited session from being
    /// mistaken for a reset contract.
    pub fn run_repeat(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
        run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        policy: Option<super::model::FailurePolicy>,
        repeat: u32,
        reset_between: bool,
    ) -> ScenarioRepeatReport {
        Self::run_repeat_with_reset(
            scenario,
            session,
            values,
            run,
            policy,
            repeat,
            if reset_between {
                ResetMode::Restart
            } else {
                ResetMode::Continue
            },
        )
    }

    /// Full repeat entry point with an explicit reset mode. Audit finding
    /// 18: a repeat against accumulated UI state is not flakiness
    /// evidence. `ResetMode::Restart` is the only contract that may
    /// classify; `Continue` produces a refusal report.
    pub fn run_repeat_with_reset(
        scenario: &Scenario,
        session: &mut crate::session::state::Session,
        values: &[ParameterValue],
        run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        policy: Option<super::model::FailurePolicy>,
        repeat: u32,
        mode: ResetMode,
    ) -> ScenarioRepeatReport {
        let repeat = repeat.max(1);
        if repeat > 1 && mode != ResetMode::Restart {
            // No reset contract: refuse the flakiness classification.
            // The executed report shows what would have run; it is not
            // counted as evidence, and the aggregate carries the refusal.
            let skipped_report = Self::failed_reset_report(
                scenario,
                "no reset contract: continuous_iterations does not support flakiness classification; use ResetMode::Restart with a scenario-owned target",
            );
            return ScenarioRepeatReport {
                repeat,
                passed_runs: 0,
                failed_runs: 1,
                verdict: FlakinessVerdict::StableFailure,
                pass_rate_pct: 0,
                first_run: Some(skipped_report),
                last_run: None,
            };
        }
        // Audit finding 24: only a scenario-owned launch may use restart
        // as a repeat reset contract. The launch spec must exist, parse,
        // and be exactly the caller-launched session's spec.
        if repeat > 1 {
            let launch = scenario.launch.as_ref().map(|l| l.to_launch_spec());
            let actual_launch = session.launch().cloned();
            // An inherited session has no reset contract at all. Never
            // classify accumulated-state repeats as flakiness evidence.
            if scenario.inherit_session || launch.is_none() {
                return ScenarioRepeatReport {
                    repeat,
                    passed_runs: 0,
                    failed_runs: 1,
                    verdict: FlakinessVerdict::StableFailure,
                    pass_rate_pct: 0,
                    first_run: Some(Self::failed_reset_report(
                        scenario,
                        "scenario-owned launch required for restart-based repeats; set inherit_session=false with launch",
                    )),
                    last_run: None,
                };
            }
            if let (Some(expected), Some(actual)) = (launch.as_ref(), actual_launch.as_ref()) {
                if expected != actual {
                    return ScenarioRepeatReport {
                        repeat,
                        passed_runs: 0,
                        failed_runs: 1,
                        verdict: FlakinessVerdict::StableFailure,
                        pass_rate_pct: 0,
                        first_run: Some(Self::failed_reset_report(
                            scenario,
                            "scenario launch mismatch: this scenario owns a target, but the session was launched from a different command/args/backend/isolation configuration",
                        )),
                        last_run: None,
                    };
                }
            }
            if mode == ResetMode::Restart && launch.is_none() {
                return ScenarioRepeatReport {
                    repeat,
                    passed_runs: 0,
                    failed_runs: 1,
                    verdict: FlakinessVerdict::StableFailure,
                    pass_rate_pct: 0,
                    first_run: Some(Self::failed_reset_report(
                        scenario,
                        "scenario-owned launch required for restart-based repeats; set inherit_session=false with launch",
                    )),
                    last_run: None,
                };
            }
        }
        let mut passed = 0u32;
        let mut failed = 0u32;
        let mut first: Option<ScenarioRunReport> = None;
        let mut last: Option<ScenarioRunReport> = None;
        for i in 0..repeat {
            if repeat > 1 {
                // Relaunch the same scenario-owned logical session: new
                // generation, so each iteration starts from the scenario's
                // own initial state (the contract finding 18 requires).
                if let Err(e) = session.restart() {
                    let report = Self::failed_reset_report(scenario, &e.to_string());
                    if first.is_none() {
                        first = Some(report.clone());
                    }
                    last = Some(report);
                    failed += 1;
                    continue;
                }
            }
            let report = Self::run_in_run_with_policy(scenario, session, values, run, policy);
            let run_passed = report.steps_failed == 0 && report.steps_skipped == 0;
            if run_passed {
                passed += 1;
            } else {
                failed += 1;
            }
            if first.is_none() {
                first = Some(report.clone());
            } else if i == repeat.saturating_sub(1) {
                last = Some(report);
            }
        }
        let verdict = if failed == 0 {
            FlakinessVerdict::StablePass
        } else if passed == 0 {
            FlakinessVerdict::StableFailure
        } else {
            FlakinessVerdict::Flaky
        };
        let pass_rate = ((passed * 100) / repeat.max(1)) as u8;
        ScenarioRepeatReport {
            repeat,
            passed_runs: passed,
            failed_runs: failed,
            verdict,
            pass_rate_pct: pass_rate,
            first_run: first,
            last_run: if repeat > 2 { last } else { None },
        }
    }

    /// A repeat report for an iteration whose reset/relaunch failed: every
    /// step is skipped with a named reason, and the run is counted as a
    /// failure (never silently merging the app's accumulated state into
    /// the flakiness claim).
    fn failed_reset_report(scenario: &Scenario, reason: &str) -> ScenarioRunReport {
        ScenarioRunReport {
            scenario_name: scenario.name.clone(),
            scenario_id: scenario.id.clone(),
            scenario_schema: scenario.schema.clone(),
            started_monotonic_ms: Some(crate::events::monotonic_ms()),
            finished_monotonic_ms: Some(crate::events::monotonic_ms()),
            status: RunStatus::StoppedOnFailure,
            steps_total: scenario.steps.len(),
            steps_passed: 0,
            steps_failed: 0,
            steps_skipped: scenario.steps.len(),
            step_results: scenario
                .steps
                .iter()
                .enumerate()
                .map(|(i, step)| StepResult {
                    index: i,
                    kind: format!("{:?}", step.kind).to_lowercase(),
                    passed: false,
                    status: StepStatus::Skipped,
                    error_category: Some(StepErrorCategory::SkippedDueToPriorFailure),
                    detail: format!(
                        "repeat reset failed: {reason}; this iteration was not attempted"
                    ),
                    transaction_seq: None,
                    elapsed_ms: None,
                })
                .collect(),
        }
    }
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
        let started_monotonic_ms = crate::events::monotonic_ms();
        let mut results = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        let mut skipped = 0;
        // P1-21/24: scenario ownership is a routing contract. A scenario
        // that owns a target may execute only when the caller has launched
        // exactly that target into the supplied session. Launch/cleanup and
        // per-repeat relaunch are the pool's lifecycle responsibility; the
        // runner cannot silently substitute an inherited session.
        if !scenario.inherit_session {
            let expected = scenario.launch.as_ref().map(|l| {
                let mut spec = l.to_launch_spec();
                spec.env
                    .retain(|(k, _)| k != "TUI_LAB_PERSONA" && k != "TERM" && k != "COLORTERM");
                spec
            });
            let mut actual = session.launch().cloned();
            if let Some(actual) = actual.as_mut() {
                actual
                    .env
                    .retain(|(k, _)| k != "TUI_LAB_PERSONA" && k != "TERM" && k != "COLORTERM");
            }
            if expected.as_ref() != actual.as_ref() {
                return ScenarioRunReport {
                    scenario_name: scenario.name.clone(),
                    scenario_id: scenario.id.clone(),
                    scenario_schema: scenario.schema.clone(),
                    started_monotonic_ms: Some(crate::events::monotonic_ms()),
                    finished_monotonic_ms: Some(crate::events::monotonic_ms()),
                    status: RunStatus::StoppedOnFailure,
                    steps_total: scenario.steps.len(),
                    steps_passed: 0,
                    steps_failed: 0,
                    steps_skipped: scenario.steps.len(),
                    step_results: scenario
                        .steps
                        .iter()
                        .enumerate()
                        .map(|(i, step)| StepResult {
                            index: i,
                            kind: format!("{:?}", step.kind).to_lowercase(),
                            passed: false,
                            status: StepStatus::Skipped,
                            error_category: Some(StepErrorCategory::InvalidParams),
                            detail: "scenario_ownership_mismatch: inherit_session=false requires the supplied session to have been launched from this scenario's exact target (launch it through the scenario runner/pool; launch/cleanup is never silently borrowed from an inherited session)".to_string(),
                            transaction_seq: None,
                            elapsed_ms: None,
                        })
                        .collect(),
                };
            }
        }
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
                    status: StepStatus::Skipped,
                    error_category: Some(StepErrorCategory::SkippedDueToPriorFailure),
                    detail: format!(
                        "skipped_due_to_prior_failure: step {at} failed and the scenario's \
                         on_failure policy is 'stop'; this step was not attempted"
                    ),
                    transaction_seq: None,
                    elapsed_ms: None,
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
                    status: StepStatus::Failed,
                    error_category: Some(StepErrorCategory::UnresolvedParameter),
                    detail: format!(
                        "unresolved_parameter: '{}' was not supplied (declare it via \
                         scenario parameters and pass its value at replay time)",
                        missing
                    ),
                    transaction_seq: None,
                    elapsed_ms: None,
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
                            kind: format!("{:?}", step.kind).to_lowercase(),
                            passed: false,
                            status: StepStatus::Failed,
                            error_category: Some(StepErrorCategory::StaleState),
                            detail: format!("stale_state: {guard_detail}"),
                            transaction_seq: None,
                            elapsed_ms: None,
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
                            kind: format!("{:?}", step.kind).to_lowercase(),
                            passed: false,
                            status: StepStatus::Failed,
                            error_category: Some(StepErrorCategory::StaleState),
                            detail: format!("stale_state: {guard_detail}"),
                            transaction_seq: None,
                            elapsed_ms: None,
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
            let step_started = std::time::Instant::now();
            let mut _executed_tx: Option<crate::execution::InteractionTransaction> = None;
            // Audit finding 23: the LEDGER SEQUENCE comes from the drive
            // outcome's committed value (set below when the run context is
            // present), never a heuristic backward scan over the ledger.
            let mut committed_ledger_seq: Option<u64> = None;
            // Audit finding 22: typed failure category per step — set by
            // each branch; the fallback (a step that ran but failed its
            // verdict) stays ExecutionError.
            let mut step_error_category = StepErrorCategory::ExecutionError;
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
                                    let outcome: Result<
                                        crate::execution::InteractionTransaction,
                                        anyhow::Error,
                                    > = match run {
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
                                            // Keep the outcome so the step can
                                            // cite the committed transaction
                                            // sequence directly.
                                            match crate::execution::drive_pipeline(
                                                session, run, spec,
                                            ) {
                                                Ok(o) => {
                                                    committed_ledger_seq = o.ledger_seq;
                                                    Ok(o.tx)
                                                }
                                                Err(e) => Err(e),
                                            }
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
                                        Ok(tx) => {
                                            let settled = tx.settled();
                                            let detail = format!(
                                                "act executed, settled={:?} ({})",
                                                tx.settle,
                                                tx.settle_reason()
                                            );
                                            _executed_tx = Some(tx);
                                            (settled, detail)
                                        }
                                        Err(e) => (false, format!("act failed: {e}")),
                                    }
                                }
                                Err(msg) => {
                                    step_error_category = StepErrorCategory::InvalidParams;
                                    (false, format!("invalid act params: {msg}"))
                                }
                            };
                        (step_passed, detail)
                    }
                    Err(e) => {
                        step_error_category = StepErrorCategory::InvalidParams;
                        (false, format!("unparseable act step: {e}"))
                    }
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
                                            Ok(out) => {
                                                if out.met {
                                                    (
                                                        true,
                                                        format!(
                                                            "event wait met={} matched_seq={}",
                                                            out.met, out.matched_seq
                                                        ),
                                                    )
                                                } else {
                                                    step_error_category =
                                                        StepErrorCategory::Timeout;
                                                    (
                                                        false,
                                                        format!(
                                                            "event wait timed out: matched_seq={}",
                                                            out.matched_seq
                                                        ),
                                                    )
                                                }
                                            }
                                            Err(e) => {
                                                step_error_category =
                                                    StepErrorCategory::ExecutionError;
                                                (false, format!("event wait failed: {e}"))
                                            }
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
                                            Ok(out) => {
                                                if out.met {
                                                    (true, format!("wait met={} reason={:?}", out.met, out.reason))
                                                } else {
                                                    step_error_category = StepErrorCategory::Timeout;
                                                    (false, format!("wait timed out: reason={:?}", out.reason))
                                                }
                                            }
                                            Err(e) => {
                                                step_error_category = StepErrorCategory::ExecutionError;
                                                (false, format!("wait failed: {e}"))
                                            }
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
                        Err(e) => {
                            step_error_category = StepErrorCategory::InvalidParams;
                            (false, format!("unparseable wait step: {e}"))
                        }
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
                                            // Audit findings 15/16/17:
                                            // - the payload completion is read from the RECORDED
                                            //   full TuiCompletionParam (object or legacy name),
                                            //   exactly as the live path converts it;
                                            // - focus moves use a FOCUS-SPECIFIC policy (bounded
                                            //   stable screen), never the caller's payload
                                            //   completion — a recorded process_exit must not
                                            //   make a Tab navigation step wait for the process
                                            //   to exit, and no_wait must not let focus assert
                                            //   before the traversal landed;
                                            // - AssertFocus produces a pending MutationGuard
                                            //   (target focus + current native/semantic revision)
                                            //   consumed by the following payload action —
                                            //   closing the assert→send TOCTOU.
                                            let mut pending_focus_guard: Option<
                                                crate::execution::MutationGuard,
                                            > = None;
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
                                                        ) = focus_step_policy();
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
                                                        // Audit finding 17: verify NOW and arm a
                                                        // guard for the payload — the guard is
                                                        // compiled from the CURRENT fresh analysis
                                                        // (structure + focus + native revision +
                                                        // semantic identity), consumed by the
                                                        // following Act through the executor's
                                                        // atomic validation.
                                                        let snapshot = match session
                                                            .snapshot_fresh()
                                                        {
                                                            Ok(a) => a,
                                                            Err(e) => {
                                                                ok = false;
                                                                why = format!(
                                                                    "focus assertion refresh failed: {e}"
                                                                );
                                                                break;
                                                            }
                                                        };
                                                        let live = snapshot
                                                            .semantic
                                                            .focus
                                                            .control_id
                                                            .as_deref();
                                                        if live != Some(target_id.as_str()) {
                                                            ok = false;
                                                            why = format!(
                                                                "focus assertion failed: expected {target_id}, focus holds {live:?}"
                                                            );
                                                            break;
                                                        }
                                                        pending_focus_guard =
                                                            Some(crate::execution::MutationGuard::capture(
                                                                Some(&snapshot),
                                                                session,
                                                                session.generation,
                                                            ));
                                                    }
                                                    crate::intent::PlannedStep::Act(action) => {
                                                        let (
                                                            payload_completion,
                                                            payload_quiet,
                                                            payload_budget,
                                                            payload_no_wait,
                                                        ) = payload_policy(&params);
                                                        let guard = pending_focus_guard.take();
                                                        if let Err(e) = drive_intent_step_guarded(
                                                            session,
                                                            run,
                                                            action,
                                                            vis,
                                                            payload_completion,
                                                            payload_quiet,
                                                            payload_budget,
                                                            payload_no_wait,
                                                            guard.as_ref(),
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
                                            step_error_category = StepErrorCategory::InvalidParams;
                                            (false, format!("invalid assertion: {d}"))
                                        } else if !p {
                                            step_error_category =
                                                StepErrorCategory::AssertionFailed;
                                            (false, d)
                                        } else {
                                            (true, d)
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

            let elapsed = step_started.elapsed().as_millis() as u64;
            // Audit finding 23: the commit's own ledger sequence (set when
            // the drive pipeline ran against a run context); the heuristic
            // backscan is gone — two identical/no-op actions can no longer
            // match the wrong transaction.
            let transaction_seq = committed_ledger_seq;
            results.push(StepResult {
                index: i,
                kind: format!("{:?}", step.kind).to_lowercase(),
                passed: step_passed,
                status: if step_passed {
                    StepStatus::Passed
                } else {
                    StepStatus::Failed
                },
                error_category: if step_passed {
                    None
                } else {
                    Some(step_error_category)
                },
                detail,
                transaction_seq,
                elapsed_ms: Some(elapsed),
            });
        }

        ScenarioRunReport {
            scenario_name: scenario.name.clone(),
            scenario_id: scenario.id.clone(),
            scenario_schema: scenario.schema.clone(),
            started_monotonic_ms: Some(started_monotonic_ms),
            finished_monotonic_ms: Some(crate::events::monotonic_ms()),
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

/// Decode the recorded intent execution policy (audit finding 15): the
/// recorded step now carries the FULL serialized [`TuiCompletionParam`]
/// (object or legacy bare name), and replay converts it EXACTLY as the
/// live path does — `to_policy()`/`quiet_ms()` — so a recorded
/// `{"type":"text_appears","text":"Saved"}` replays as
/// `TextAppears("Saved")`, never `TextAppears("")`.
fn intent_execution_policy(
    params: &serde_json::Value,
) -> (crate::capture::CompletionPolicy, u64, u64, bool) {
    use crate::mcp::params::TuiCompletionParam;
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
    // Full wire value first (audit finding 15); legacy bare-name strings
    // migrate through TuiCompletionParam::from_recorded.
    let recorded = params.get("completion").unwrap_or(&serde_json::Value::Null);
    let completion = TuiCompletionParam::from_recorded(recorded)
        .map(|c| {
            let quiet_override = c.quiet_ms();
            quiet_override
        })
        .and_then(|_| TuiCompletionParam::from_recorded(recorded).map(|c| c.to_policy()))
        .unwrap_or(crate::capture::CompletionPolicy::StableScreen);
    (completion, quiet, budget, no_wait)
}

/// Audit finding 16: focus navigation policy — a Tab/Shift+Tab traversal
/// step waits for the FOCUS/SEMANTIC transition or a stable screen under
/// a bounded INTERNAL budget, and NEVER inherits the caller's recorded
/// payload completion (`process_exit` must not make a Tab step wait for
/// the process to exit; `no_wait` must not let a focus assertion read
/// pre-traversal state).
fn focus_step_policy() -> (crate::capture::CompletionPolicy, u64, u64, bool) {
    (
        crate::capture::CompletionPolicy::StableScreen,
        300,
        1300,
        false,
    )
}

/// Audit finding 16: the payload action uses the caller-RECORDED
/// completion/quiet/budget/no_wait — the semantics that were actually
/// recorded — decoded from the full serialized completion.
fn payload_policy(
    params: &serde_json::Value,
) -> (crate::capture::CompletionPolicy, u64, u64, bool) {
    intent_execution_policy(params)
}

/// [`drive_intent_step`] with an optional pending [`MutationGuard`]
/// (audit finding 17): the guard armed by AssertFocus is consumed by the
/// following payload Act and validated atomically with the send by the
/// executor — closing the assert→send TOCTOU on replay.
#[allow(clippy::too_many_arguments)]
fn drive_intent_step_guarded(
    session: &mut crate::session::state::Session,
    run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
    action: &crate::execution::CanonicalAction,
    vis: crate::execution::InputVisibility,
    completion: crate::capture::CompletionPolicy,
    quiet_ms: u64,
    budget_ms: u64,
    no_wait: bool,
    guard: Option<&crate::execution::MutationGuard>,
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
                guard,
                scenario: None,
                origin: crate::execution::DriveOrigin::Scenario,
                ticket,
            };
            crate::execution::drive_pipeline(session, run, spec).map(|o| o.tx)
        }
        None => crate::execution::execute_act_with_guard_and_origin(
            session,
            crate::execution::DriveOrigin::Scenario,
            action,
            quiet_ms,
            budget_ms,
            no_wait,
            vis,
            completion,
            guard,
        ),
    }
}

/// One focus-secured intent step at replay time via the same driving
/// pipeline the live intent path uses. Guard-less; see
/// [`drive_intent_step_guarded`] for the asserted form.
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
    drive_intent_step_guarded(
        session, run, action, vis, completion, quiet_ms, budget_ms, no_wait, None,
    )
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
    // Audit P1-20/21: the V2 expectation fields (generation, native
    // revision, fused semantic identity) ride the compiled guard, so a
    // replay precondition is as strong as the live execution's guard —
    // native-only drift (focus moved, pixels unchanged) fails replay too.
    // The V2 visible/history text split maps to the guard's viewport-only
    // text predicate and the history check (checked pre-send below via
    // check_expect, since MutationGuard has no history slot).
    let guard = crate::execution::MutationGuard {
        structure_hash: expect.structure_hash.clone(),
        focus_control_id: expect.focus_control_id.clone(),
        text_visible: expect
            .visible_text_present
            .clone()
            .or_else(|| expect.text_present.clone()),
        generation: expect.generation,
        native_revision: expect.native_revision,
        semantic_identity: expect.semantic_identity.clone(),
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
    // Audit finding 21: replay preconditions evaluate against a FRESH
    // pre-dispatch snapshot — never `session.last()`, which can be a
    // seconds-old explicit observation. Wait/assert/intent preconditions
    // get the same freshness rule as act steps.
    let analysis = match session.snapshot_fresh() {
        Ok(a) => a,
        Err(e) => return (false, format!("no fresh frame to verify against: {e}")),
    };
    let screen = &analysis.frame;
    if let Some(want_gen) = expect.generation {
        if session.generation != want_gen {
            return (
                false,
                format!(
                    "session restarted since capture (generation {} -> {})",
                    want_gen, session.generation
                ),
            );
        }
    }
    if let Some(want_rev) = expect.native_revision {
        if session.native_revision() != Some(want_rev) {
            return (
                false,
                format!(
                    "native semantic revision changed since capture: expected {want_rev}, live {:?}",
                    session.native_revision()
                ),
            );
        }
    }
    if let Some(want_identity) = &expect.semantic_identity {
        if &analysis.semantic_identity != want_identity {
            return (
                false,
                format!(
                    "fused semantic identity changed since capture: expected {want_identity}, live {}",
                    analysis.semantic_identity
                ),
            );
        }
    }
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
        if analysis.semantic.focus.control_id.as_deref() != Some(want_focus.as_str()) {
            return (
                false,
                format!(
                    "focus moved since capture: expected {want_focus}, live {:?}",
                    analysis.semantic.focus.control_id
                ),
            );
        }
    }
    // V2 split text: visible checks the viewport ONLY (Matching the
    // MutationGuard.text_visible semantics — audit P1-20); history checks
    // scrollback; legacy `text_present` keeps the old viewport-OR-history
    // semantics for V1 scenarios.
    if let Some(text) = &expect.visible_text_present {
        if !crate::capture::visible_text_contains(screen, text) {
            return (false, format!("required visible text absent: {text:?}"));
        }
    }
    if let Some(text) = &expect.history_text_present {
        if !crate::capture::history_text_contains(screen, text) {
            return (false, format!("required history text absent: {text:?}"));
        }
    }
    if let Some(text) = &expect.text_present {
        let visible = crate::capture::visible_text_contains(screen, text);
        let historical = crate::capture::history_text_contains(screen, text);
        // A Type payload recorded at capture time describes the action's
        // identity, not a precondition that already held. If it was absent
        // before dispatch, requiring it now would make every same-app
        // fresh-session replay fail. Require a recorded marker only when
        // it actually held at capture; legacy files without the marker
        // retain their permissive migration semantics.
        let required = expect.recorded_visible.is_some();
        let present = if expect.recorded_visible == Some(true) {
            visible
        } else if required {
            visible || historical
        } else {
            true
        };
        if !present {
            let location = if expect.recorded_visible == Some(true) {
                "visible"
            } else {
                "visible/history"
            };
            return (
                false,
                format!("required text absent ({location}): {text:?}"),
            );
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

/// Audit finding 50: independently relaunch the same scenario-owned target
/// once per selected terminal persona. Persona is the only changing
/// dimension — same scenario, parameters, policy, dimensions and build — and
/// every execution gets a fresh session generation. The runner owns
/// provenance and refusal contracts; launch/execution are supplied by the
/// environment adapter (async pool callers await inside their adapter).
///
/// `make_persona_run` returns the matrix outcome and an optional cleanup id.
/// Inherited scenarios and unknown personas are refused before launch.
pub fn run_persona_matrix_selected<T, E: From<String> + Send + Sync + 'static>(
    scenario: &Scenario,
    persona_ids: &[String],
    values: &[ParameterValue],
    run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
    policy: Option<super::model::FailurePolicy>,
    mut make_persona_run: impl FnMut(
        &Scenario,
        &crate::terminal::TerminalPersona,
        &crate::session::state::LaunchSpec,
        &[ParameterValue],
        Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        Option<super::model::FailurePolicy>,
    ) -> Result<
        (crate::terminal::PersonaMatrixOutcome<T>, Option<String>),
        E,
    >,
) -> crate::terminal::PersonaMatrixExecution<T, E> {
    let mut outcomes = Vec::with_capacity(persona_ids.len());
    let mut cleanup_ids = Vec::with_capacity(persona_ids.len());
    let mut launches_passed = true;

    if scenario.inherit_session || scenario.launch.is_none() {
        launches_passed = false;
        for persona_id in persona_ids {
            let persona = dummy_persona(persona_id);
            let row: crate::terminal::PersonaMatrixRow<Result<T, String>> =
                crate::terminal::PersonaMatrixRow {
                    persona: persona.id.clone(),
                    term: persona.term.clone(),
                    colorterm: persona.colorterm.clone(),
                    outcome: Err(
                        "scenario-owned launch required: set inherit_session=false with launch"
                            .to_string(),
                    ),
                };
            outcomes.push(Err(E::from(match row.outcome {
                Err(message) => message,
                Ok(_) => unreachable!("refusal rows always carry an error payload"),
            })));
        }
        return crate::terminal::PersonaMatrixExecution {
            outcomes,
            cleanup_ids,
            launches_passed,
        };
    }

    let personas = crate::terminal::TerminalPersona::builtin();
    for persona_id in persona_ids {
        let Some(persona) = personas.iter().find(|p| &p.id == persona_id) else {
            launches_passed = false;
            let persona = dummy_persona(persona_id);
            let row: crate::terminal::PersonaMatrixRow<Result<T, String>> =
                crate::terminal::PersonaMatrixRow {
                    persona: persona.id.clone(),
                    term: persona.term.clone(),
                    colorterm: persona.colorterm.clone(),
                    outcome: Err(format!("unknown terminal persona '{persona_id}'")),
                };
            outcomes.push(Err(E::from(match row.outcome {
                Err(message) => message,
                Ok(_) => unreachable!("refusal rows always carry an error payload"),
            })));
            continue;
        };
        let mut spec = scenario
            .launch
            .as_ref()
            .expect("checked launch")
            .to_ownership_spec();
        persona.apply_env(&mut spec.env);
        if let Some(pair) = spec.env.iter_mut().find(|(k, _)| k == "TUI_LAB_PERSONA") {
            pair.1 = persona.id.clone();
        } else {
            spec.env
                .push(("TUI_LAB_PERSONA".to_string(), persona.id.clone()));
        }
        match make_persona_run(scenario, persona, &spec, values, run, policy) {
            Ok((outcome, cleanup)) => {
                if let Some(id) = cleanup {
                    cleanup_ids.push(id);
                }
                outcomes.push(Ok(outcome));
            }
            Err(error) => {
                launches_passed = false;
                outcomes.push(Err(error));
            }
        }
    }

    crate::terminal::PersonaMatrixExecution {
        outcomes,
        cleanup_ids,
        launches_passed,
    }
}

/// Canonical all-builtins convenience over [`run_persona_matrix_selected`].
pub fn run_persona_matrix<T, E: From<String> + std::error::Error + Send + Sync + 'static>(
    scenario: &Scenario,
    values: &[ParameterValue],
    run: Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
    policy: Option<super::model::FailurePolicy>,
    make_persona_run: impl FnMut(
        &Scenario,
        &crate::terminal::TerminalPersona,
        &crate::session::state::LaunchSpec,
        &[ParameterValue],
        Option<&std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>>,
        Option<super::model::FailurePolicy>,
    ) -> Result<
        (crate::terminal::PersonaMatrixOutcome<T>, Option<String>),
        E,
    >,
) -> crate::terminal::PersonaMatrixExecution<T, E> {
    let requested: Vec<String> = crate::terminal::TerminalPersona::builtin()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    run_persona_matrix_selected(scenario, &requested, values, run, policy, make_persona_run)
}

fn dummy_persona(id: &str) -> crate::terminal::TerminalPersona {
    crate::terminal::TerminalPersona {
        id: id.to_string(),
        term: id.to_string(),
        colorterm: None,
        color_depth: String::new(),
        supports: Vec::new(),
        env: Vec::new(),
    }
}
