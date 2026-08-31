//! Live reproduction minimization (Wave D item 38).
//!
//! The [`ReproMinimizer`] was fully built and connected to nothing: it
//! needed a caller willing to (a) capture the trace that failed, (b) restart
//! the app clean before every candidate replay, (c) re-execute the candidate
//! through the one canonical executor, and (d) declare failure honestly.
//! This module is that caller.
//!
//! Pipeline: crash observed → take the executed steps as the candidate
//! trace → delta-debug over restart-replay attempts → the minimal
//! still-failing subsequence becomes a [`crate::scenario::model::Scenario`]
//! saved into the run → its ID rides on the Finding as `reproduction`, so a
//! human (or another agent) can replay the failure with one call.

use crate::execution::{execute_act, CanonicalAction};
use crate::exploration::random::ExplorationStep;
use crate::exploration::repro_minimizer::{ReproAction, ReproResult};
use crate::scenario::model::Scenario;
use crate::session::state::Session;

/// How a replay attempt failed, classified for the finding's evidence.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The child process exited with a nonzero code or signal.
    Crash,
    /// The process died for an unclassifiable reason.
    ProcessDied,
    /// Reproduction could not be attempted (session/executor error).
    ExecutionError,
}

impl FailureKind {
    /// Detect a failure from a post-action frame: process death is the
    /// honest, cheap signal (a UI assertion could false-positive on
    /// unrelated screens; a dead process cannot).
    pub fn from_process(
        running: bool,
        exit_code: Option<i32>,
        exit_signal: Option<&str>,
    ) -> Option<Self> {
        if running {
            return None;
        }
        let crashed = exit_code.map(|c| c != 0).unwrap_or(true) || exit_signal.is_some();
        Some(if crashed {
            FailureKind::Crash
        } else {
            FailureKind::ProcessDied
        })
    }
}

/// The result of a full minimization pipeline.
#[derive(Debug, serde::Serialize)]
pub struct ReproPipeline {
    /// Steps in the original trace.
    pub original_len: usize,
    /// Steps in the minimized reproduction (0 when nothing reproduced).
    pub minimized_len: usize,
    pub reproduced: bool,
    pub failure: Option<FailureKind>,
    /// The replayable minimized reproduction, built and ready to save into
    /// the run (item 38). `Some` only when the minimized trace was
    /// confirmed to still fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<Scenario>,
    /// Human-readable step summary for the finding's evidence.
    pub steps: Vec<String>,
    /// Replay attempts made during minimization (cost of the pipeline).
    pub attempts: u32,
}

impl ReproPipeline {
    /// The scenario ID — the Finding's `reproduction` value.
    pub fn scenario_id(&self) -> Option<String> {
        self.scenario.as_ref().map(|s| s.id.clone())
    }
}

/// Convert executed exploration steps into replayable [`ReproAction`]s.
/// Steps whose canonical payload was redacted (sensitive) are skipped — a
/// redacted action cannot be replayed losslessly, and a reproduction built
/// on a fake placeholder would be a lie.
pub fn trace_from_steps(steps: &[ExplorationStep]) -> Vec<ReproAction> {
    steps
        .iter()
        .filter_map(|s| {
            let action = replayable_action(&s.action)?;
            Some(ReproAction {
                index: s.seq as u32,
                action,
            })
        })
        .collect()
}

/// Rebuild a [`CanonicalAction`] from a recorded action name. The random
/// explorer's pool is a fixed vocabulary of key presses; this inverts it.
/// Returns `None` for names that cannot be reconstructed.
fn replayable_action(name: &str) -> Option<CanonicalAction> {
    use crate::backend::{KeyCode, KeyEvent, KeyModifiers};
    let key = |c: KeyCode| CanonicalAction::Key {
        key: KeyEvent::new(c),
    };
    match name {
        "tab" => Some(key(KeyCode::Tab)),
        "shift+tab" => Some(CanonicalAction::Key {
            key: KeyEvent::with_modifiers(KeyCode::Tab, KeyModifiers::SHIFT),
        }),
        "down" => Some(key(KeyCode::Down)),
        "up" => Some(key(KeyCode::Up)),
        "right" => Some(key(KeyCode::Right)),
        "left" => Some(key(KeyCode::Left)),
        "enter" => Some(key(KeyCode::Enter)),
        "escape" => Some(key(KeyCode::Escape)),
        "pageup" => Some(key(KeyCode::PageUp)),
        "pagedown" => Some(key(KeyCode::PageDown)),
        "home" => Some(key(KeyCode::Home)),
        "end" => Some(key(KeyCode::End)),
        "space" => Some(key(KeyCode::Char(' '))),
        _ => None,
    }
}

/// Test one candidate subsequence: clean restart, replay every action, and
/// report whether the failure persists. This is the callback handed to the
/// delta-debugging minimizer.
fn test_candidate(
    session: &mut Session,
    candidate: &[ReproAction],
    expected: &FailureKind,
    attempts: &mut u32,
) -> ReproResult {
    *attempts += 1;
    // Clean state before the attempt: restart from the same LaunchSpec.
    // A reproduction that needs a dirty prior state is not a reproduction.
    if let Err(e) = session.restart() {
        return ReproResult {
            actions_executed: 0,
            reproduced: false,
            failure_type: Some(format!("restart failed: {e}")),
        };
    }
    // Post-restart frame is the baseline.
    let _ = session.observe(50);

    for ra in candidate {
        match execute_act(session, &ra.action, 80, 600, false) {
            Ok(tx) => {
                let after = tx.after();
                if let Some(kind) = FailureKind::from_process(
                    after.process.running,
                    after.process.exit_code,
                    after.process.exit_signal.as_deref(),
                ) {
                    // Failure observed. Same kind as the original?
                    let same = &kind == expected;
                    return ReproResult {
                        actions_executed: ra.index + 1,
                        reproduced: same,
                        failure_type: Some(format!("{kind:?}")),
                    };
                }
            }
            Err(e) => {
                // Executor error mid-replay: honest — not a reproduction.
                return ReproResult {
                    actions_executed: ra.index,
                    reproduced: false,
                    failure_type: Some(format!("ExecutionError: {e}")),
                };
            }
        }
    }
    ReproResult {
        actions_executed: candidate.len() as u32,
        reproduced: false,
        failure_type: None,
    }
}

/// Full pipeline: verify the original trace reproduces, minimize it, and
/// build the saved [`Scenario`]. The caller saves `pipeline.scenario` into
/// the run and attaches `scenario_id()` to its Finding.
///
/// When the original trace does not reproduce on a clean restart (flaky, or
/// the failure needed the dirty state), the pipeline reports that honestly —
/// no scenario is fabricated from a trace that does not fail.
pub fn minimize_crash(
    session: &mut Session,
    steps: &[ExplorationStep],
    expected_failure: FailureKind,
    name: &str,
) -> ReproPipeline {
    let trace = trace_from_steps(steps);
    let original_len = trace.len();
    if trace.is_empty() {
        return ReproPipeline {
            original_len: 0,
            minimized_len: 0,
            reproduced: false,
            failure: Some(expected_failure),
            scenario: None,
            steps: Vec::new(),
            attempts: 0,
        };
    }

    let attempts_total = std::cell::Cell::new(0u32);
    // Wrap the session in a cell: the minimizer's callback needs `&mut`.
    let mut session_opt = Some(session);
    let mut minimizer =
        crate::exploration::repro_minimizer::ReproMinimizer::new(|candidate: &[ReproAction]| {
            let s = session_opt
                .as_mut()
                .expect("session available during minimize");
            let mut attempts = attempts_total.take();
            let r = test_candidate(s, candidate, &expected_failure, &mut attempts);
            attempts_total.set(attempts);
            r
        });

    let minimal = minimizer.minimize(&trace);
    let attempts = attempts_total.get();

    // Confirm the minimized sequence still fails on one more clean restart
    // (the minimizer's last successful test may have been a lucky prefix).
    let reproduced = if minimal.is_empty() {
        false
    } else {
        let s = session_opt.as_mut().expect("session");
        let mut attempts_final = 0;
        test_candidate(s, &minimal, &expected_failure, &mut attempts_final).reproduced
    };

    let step_summaries: Vec<String> = minimal
        .iter()
        .map(|ra| format!("#{} {}", ra.index, ra.name()))
        .collect();

    let scenario = if reproduced {
        Some(build_scenario(name, &minimal, &expected_failure))
    } else {
        None
    };

    ReproPipeline {
        original_len,
        minimized_len: minimal.len(),
        reproduced,
        failure: if reproduced {
            Some(expected_failure)
        } else {
            None
        },
        scenario,
        steps: step_summaries,
        attempts: attempts + 1,
    }
}

/// Build the replayable Scenario for a minimized trace (item 38). Steps are
/// the tagged canonical actions — the same JSON shape a live `tui_act` call
/// carried, so `tui_scenario run` replays them through the identical
/// executor path.
pub fn build_scenario(name: &str, minimal: &[ReproAction], failure: &FailureKind) -> Scenario {
    let scenario = Scenario::new(format!("repro-{name}"));
    let mut with_meta = scenario;
    with_meta.metadata = Some(crate::scenario::model::ScenarioMetadata {
        description: Some(format!(
            "Minimized reproduction ({failure:?}) produced by tui_explore crash minimization"
        )),
        tags: vec!["repro".into(), "crash".into()],
        created_at: None,
        source: Some("tui_explore/repro".into()),
    });
    let mut out = with_meta;
    for ra in minimal {
        let params = serde_json::to_value(&ra.action).unwrap_or_default();
        out = out.act(params);
    }
    // The reproduction's assertion: the process should be dead at the end
    // (that is what made it a reproduction).
    out = out.assert(serde_json::json!({
        "assertion": "process_exit",
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(seq: u64, action: &str) -> ExplorationStep {
        ExplorationStep {
            seq,
            action: action.to_string(),
            before: crate::exploration::state_graph::StateIdentity::from_parts("b"),
            after: crate::exploration::state_graph::StateIdentity::from_parts("a"),
            changed: true,
            settle: crate::execution::SettleStatus::Met,
            elapsed_ms: 5,
            novel_state: false,
            process_running: true,
        }
    }

    /// Trace conversion: named pool actions rebuild losslessly; unknown or
    /// redacted names are dropped rather than replayed as garbage.
    #[test]
    fn trace_conversion_filters_unknown_names() {
        let steps = vec![step(0, "tab"), step(1, "enter"), step(2, "mouse_click")];
        let trace = trace_from_steps(&steps);
        assert_eq!(trace.len(), 2, "mouse_click has no key-pool reconstruction");
        assert_eq!(trace[0].index, 0);
        assert_eq!(trace[1].index, 1);
        assert!(matches!(trace[1].action, CanonicalAction::Key { .. }));
    }

    /// Item 38: the pipeline confirms reproduction before saving a
    /// scenario. These tests run the pure parts; the live restart-replay
    /// path is exercised end-to-end by the e2e suite against the fixture.
    #[tokio::test]
    async fn empty_trace_never_reproduces() {
        // A real session (cheap python sleeper): the empty trace must
        // short-circuit before it is touched. Actor-backed launch (the
        // legacy blocking SessionManager is removed).
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('repro-empty'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let pipeline = pool
            .with_session(Some(&id), |s| minimize_crash(s, &[], FailureKind::Crash, "t"))
            .await
            .expect("actor run");
        assert!(!pipeline.reproduced);
        assert!(pipeline.scenario_id().is_none());
        assert_eq!(pipeline.original_len, 0);
        pool.stop(&id).await.ok();
    }

    /// The saved scenario shape: acts carry tagged canonical JSON; the
    /// final step asserts process exit (the reproduction's claim).
    #[test]
    fn scenario_is_replayable_and_asserts_exit() {
        let minimal = vec![
            ReproAction {
                index: 3,
                action: CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Tab),
                },
            },
            ReproAction {
                index: 7,
                action: CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Enter),
                },
            },
        ];
        let s = build_scenario("settings-crash", &minimal, &FailureKind::Crash);
        assert!(s.name.starts_with("repro-"));
        assert_eq!(s.step_count(), 3, "2 acts + 1 exit assert");
        let last = s.steps.last().unwrap();
        assert_eq!(last.kind, crate::scenario::model::StepKind::Assert);
        assert_eq!(last.params["assertion"], "process_exit");
        // Act steps carry the tagged canonical payload (replay-fidelity).
        let first_act = &s.steps[0];
        assert_eq!(first_act.params["kind"], "key");
        assert!(s.is_valid());
    }

    /// FailureKind classification honesty: a clean exit (code 0) is not a
    /// crash; a signal is; a nonzero code is.
    #[test]
    fn failure_classification() {
        assert_eq!(FailureKind::from_process(true, None, None), None);
        assert_eq!(
            FailureKind::from_process(false, Some(0), None),
            Some(FailureKind::ProcessDied),
            "clean exit while exploring is death, not a crash"
        );
        assert_eq!(
            FailureKind::from_process(false, Some(101), None),
            Some(FailureKind::Crash)
        );
        assert_eq!(
            FailureKind::from_process(false, None, Some("SIGSEGV")),
            Some(FailureKind::Crash)
        );
    }
}
