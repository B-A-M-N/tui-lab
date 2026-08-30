//! Seeded deterministic random exploration (spec section 4.2). Same app + same
//! seed => same action sequence when practical. Records everything for replay.
//!
//! Re-review items 12/13:
//!   * The state graph records WHAT ACTUALLY HAPPENED: each step emits an
//!     ordered [`ExplorationStep`] (seq, action, before/after identity,
//!     transaction outcome) while the action runs — no post-hoc hash
//!     reconstruction.
//!   * [`ExplorationBudget`] is the authority: every iteration checks all
//!     applicable limits and the report names the real completion reason
//!     ([`ExplorationCompletionReason`]), never a generic "completed".
//!   * Relaunch goes through `session.restart()` so generation increments and
//!     lifecycle handling stays centralized.

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::path::Path;
use std::time::Instant;

use crate::backend::{KeyEvent, KeyModifiers};
use crate::execution::{execute_act, CanonicalAction};
use crate::session::state::Session;

/// Why an exploration actually stopped (re-review item 13).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationCompletionReason {
    ActionBudget,
    TimeBudget,
    RelaunchBudget,
    DepthBudget,
    UniqueStateBudget,
    CleanExit,
    Failure,
    Cancelled,
}

/// One executed exploration step, recorded while it happened (item 12).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExplorationStep {
    /// Ordered execution index (0-based).
    pub seq: u64,
    /// Canonical action name that was sent.
    pub action: String,
    /// Structure hash observed immediately before the action.
    pub before: String,
    /// Structure hash observed after the action settled.
    pub after: String,
    /// Whether the screen actually changed (before != after).
    pub changed: bool,
    /// Whether the action's anchored settle wait reached stability.
    pub settled: bool,
    /// Settle wait elapsed time in ms.
    pub elapsed_ms: u64,
    /// Novelty of the after-state at execution time (true = first visit).
    pub novel_state: bool,
    /// Process state after the step.
    pub process_running: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct ExploreReport {
    pub seed: u64,
    pub actions_run: u32,
    pub actions_requested: u32,
    pub screens_seen: usize,
    pub structure_hashes: Vec<String>,
    /// Ordered record of every executed step (item 12).
    pub steps: Vec<ExplorationStep>,
    /// Why the loop actually stopped (item 13).
    pub completion_reason: ExplorationCompletionReason,
    /// Number of relaunches performed (via session.restart()).
    pub relaunches: u32,
    pub exits: Vec<ProcessExit>,
    pub novel_transitions: u32,
    pub terminated_early: bool,
    pub elapsed_ms: u64,
    pub recording_path: Option<String>,
    pub recording_events: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct ProcessExit {
    pub action_index: u32,
    pub action_name: String,
    pub classification: ExitClassification,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitClassification {
    Clean,
    Error,
    Signal,
    Unknown,
}

fn classify_exit(code: Option<i32>, signal: Option<&str>) -> ExitClassification {
    if let Some(_sig) = signal {
        return ExitClassification::Signal;
    }
    match code {
        Some(0) => ExitClassification::Clean,
        Some(_) => ExitClassification::Error,
        None => ExitClassification::Unknown,
    }
}

fn key(code: crate::backend::KeyCode) -> CanonicalAction {
    CanonicalAction::Key {
        key: KeyEvent::new(code),
    }
}

/// An action factory: name + zero-arg canonical action constructor. Actions
/// are typed ([`CanonicalAction`], Wave-2 item 10) so an exploration trace
/// can be replayed losslessly — the old `(name, fn() -> Input)` pool lost
/// modifiers/coords at recording time.
type ActionFactory = (&'static str, fn() -> CanonicalAction);

const ACTION_POOL: &[ActionFactory] = &[
    ("tab", || key(crate::backend::KeyCode::Tab)),
    ("shift+tab", || CanonicalAction::Key {
        key: KeyEvent::with_modifiers(crate::backend::KeyCode::Tab, KeyModifiers::SHIFT),
    }),
    ("down", || key(crate::backend::KeyCode::Down)),
    ("up", || key(crate::backend::KeyCode::Up)),
    ("right", || key(crate::backend::KeyCode::Right)),
    ("left", || key(crate::backend::KeyCode::Left)),
    ("enter", || key(crate::backend::KeyCode::Enter)),
    ("escape", || key(crate::backend::KeyCode::Escape)),
    ("pageup", || key(crate::backend::KeyCode::PageUp)),
    ("pagedown", || key(crate::backend::KeyCode::PageDown)),
    ("home", || key(crate::backend::KeyCode::Home)),
    ("end", || key(crate::backend::KeyCode::End)),
    ("space", || key(crate::backend::KeyCode::Char(' '))),
];

/// All applicable budget limits for one exploration (item 13).
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_actions: u32,
    pub max_runtime_ms: u64,
    pub max_relaunches: u32,
    pub max_depth: u32,
    pub max_unique_states: u32,
}

impl Budget {
    /// From the run graph's [`crate::exploration::state_graph::ExplorationBudget`]
    /// — the budget is the authority, callers do not pass bare `actions: u32`.
    pub fn from_graph_budget(b: &crate::exploration::state_graph::ExplorationBudget) -> Self {
        Budget {
            max_actions: b.max_actions,
            max_runtime_ms: b.max_runtime_ms,
            max_relaunches: b.max_relaunches,
            max_depth: b.max_depth,
            max_unique_states: b.max_unique_states,
        }
    }
}

/// Run a seeded random exploration. Returns a report with evidence, including
/// the ordered step record and the true completion reason.
pub fn run(
    session: &mut Session,
    seed: u64,
    budget: Budget,
    recording_path: Option<&Path>,
) -> anyhow::Result<ExploreReport> {
    let started = Instant::now();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut hashes: Vec<String> = Vec::new();
    let mut steps: Vec<ExplorationStep> = Vec::new();
    let mut exits = Vec::new();
    let mut novel = 0u32;
    let mut last_hash: Option<String> = None;
    let mut relaunches = 0u32;
    let mut terminated_early = false;

    // The completion reason when the loop exits naturally after the last
    // action. Overridden by whichever budget (or failure) actually stops us.
    let mut reason = ExplorationCompletionReason::CleanExit;

    for i in 0..budget.max_actions {
        // ── Budget checks: every applicable limit, every iteration (item 13).
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed >= budget.max_runtime_ms {
            reason = ExplorationCompletionReason::TimeBudget;
            break;
        }
        if relaunches >= budget.max_relaunches {
            reason = ExplorationCompletionReason::RelaunchBudget;
            break;
        }
        if hashes.len() as u32 >= budget.max_unique_states {
            reason = ExplorationCompletionReason::UniqueStateBudget;
            break;
        }
        if steps.len() as u32 >= budget.max_depth {
            reason = ExplorationCompletionReason::DepthBudget;
            break;
        }

        let (name, mk) = ACTION_POOL.choose(&mut rng).unwrap();
        // Go through the canonical executor so baseline-before-send and
        // anchored settle-wait ordering match MCP `tui_act` exactly
        // (re-review P0 "one canonical executor").
        let action = mk();
        let tx = match execute_act(session, &action, 120, 1500, false) {
            Ok(tx) => tx,
            Err(_e) => {
                exits.push(ProcessExit {
                    action_index: i,
                    action_name: name.to_string(),
                    classification: ExitClassification::Unknown,
                    exit_code: None,
                    exit_signal: None,
                });
                terminated_early = true;
                reason = ExplorationCompletionReason::Failure;
                break;
            }
        };

        let settled = tx.settled;
        let after = tx.after.clone();
        let elapsed_ms = tx.elapsed_ms;
        let before_hash = tx.before.structure_hash.clone();

        let novel_state = !hashes.contains(&after.structure_hash);
        if novel_state {
            hashes.push(after.structure_hash.clone());
        }
        if last_hash.as_deref() != Some(&after.structure_hash) {
            novel += 1;
            last_hash = Some(after.structure_hash.clone());
        }

        // Ordered step record — emitted while the action happened (item 12).
        steps.push(ExplorationStep {
            seq: i as u64,
            action: name.to_string(),
            before: before_hash.clone(),
            after: after.structure_hash.clone(),
            changed: before_hash != after.structure_hash,
            settled,
            elapsed_ms,
            novel_state,
            process_running: after.process.running,
        });

        // Classify process exit properly (item 43).
        if !after.process.running {
            let classification = classify_exit(
                after.process.exit_code,
                after.process.exit_signal.as_deref(),
            );
            exits.push(ProcessExit {
                action_index: i,
                action_name: name.to_string(),
                classification,
                exit_code: after.process.exit_code,
                exit_signal: after.process.exit_signal.clone(),
            });

            // Relaunch through restart(): same logical session, generation
            // increments, lifecycle handling stays centralized (item 13).
            if relaunches + 1 > budget.max_relaunches {
                reason = ExplorationCompletionReason::RelaunchBudget;
                terminated_early = true;
                break;
            }
            match session.restart() {
                Ok(()) => {
                    relaunches += 1;
                    // Post-restart frame is the new baseline.
                    let _ = session.observe(30)?;
                    last_hash = None;
                }
                Err(_e) => {
                    terminated_early = true;
                    reason = ExplorationCompletionReason::Failure;
                    break;
                }
            }
        }
    }

    // If we consumed the full action allowance, name the action budget.
    if !terminated_early
        && steps.len() as u32 >= budget.max_actions
        && reason == ExplorationCompletionReason::CleanExit
    {
        reason = ExplorationCompletionReason::ActionBudget;
    }

    // Write recording if requested.
    let mut final_path = None;
    let event_count = session.recording_event_count() as u32;
    if let Some(path) = recording_path {
        if session.is_recording() {
            session.write_recording(path)?;
            final_path = Some(path.to_string_lossy().to_string());
        }
    }

    Ok(ExploreReport {
        seed,
        actions_run: steps.len() as u32,
        actions_requested: budget.max_actions,
        screens_seen: hashes.len(),
        structure_hashes: hashes,
        steps,
        completion_reason: reason,
        relaunches,
        exits,
        novel_transitions: novel,
        terminated_early,
        elapsed_ms: started.elapsed().as_millis() as u64,
        recording_path: final_path,
        recording_events: event_count,
    })
}

/// Feed executed steps into the run's state graph — from the ordered record
/// of what actually happened, not from reconstructed hashes (item 12).
pub fn record_steps(
    graph: &mut crate::exploration::state_graph::StateGraph,
    steps: &[ExplorationStep],
) {
    for s in steps {
        graph.record_transition(&s.before, &s.after, &s.action);
    }
}
