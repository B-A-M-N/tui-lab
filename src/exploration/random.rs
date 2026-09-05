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
use crate::execution::CanonicalAction;
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
///
/// Identity-bearing (re-review P0 fix 3): before/after are full
/// [`StateIdentity`] values, not bare structure hashes, so the graph
/// distinguishes states that differ only in interaction state (same text,
/// focus on "Save" vs focus on "Cancel"). Semantic analysis runs per step —
/// the same `analyze` the MCP observe path uses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExplorationStep {
    /// Ordered execution index (0-based).
    pub seq: u64,
    /// Canonical action name that was sent.
    pub action: String,
    /// Layered identity observed immediately before the action.
    pub before: crate::exploration::state_graph::StateIdentity,
    /// Layered identity observed after the action settled.
    pub after: crate::exploration::state_graph::StateIdentity,
    /// Whether the screen actually changed (before.id() != after.id()).
    pub changed: bool,
    /// How the action's anchored settle wait resolved (re-review P1 fix 8).
    pub settle: crate::execution::SettleStatus,
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
    /// Every unique layered identity seen (P0 fix 3) — interaction-distinct
    /// states count separately, matching what the graph records.
    pub unique_state_identities: Vec<crate::exploration::state_graph::StateIdentity>,
    /// Ordered record of every executed step (item 12).
    pub steps: Vec<ExplorationStep>,
    /// Why the loop actually stopped (item 13).
    pub completion_reason: ExplorationCompletionReason,
    /// Number of relaunches performed (via session.restart()).
    pub relaunches: u32,
    pub exits: Vec<ProcessExit>,
    pub novel_transitions: u32,
    /// Item 29: per-dimension novelty scoreboard — WHAT the run explored,
    /// not just how many steps ran.
    pub novelty: crate::exploration::novelty::NoveltySummary,
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
    /// Highest interaction-risk class the autonomous driver may execute
    /// (re-review items 27/28). Pool actions whose class exceeds this are
    /// excluded before any draw — Escape (Unknown by default) is absent
    /// from a `mutating`-allowance run, and a `safe` allowance additionally
    /// drops enter/space.
    pub allowed_risk: crate::intent::ActionRisk,
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
            // Autonomous default: the driving-risk fence — Unknown is
            // excluded unless the caller proves otherwise (item 28).
            allowed_risk: crate::intent::ActionRisk::Mutating,
        }
    }

    /// The pool actions this budget's risk allowance admits, with their
    /// classified classes (item 27: every driver-visible action names its
    /// class). Deterministic order (pool order).
    pub fn admitted_pool(&self) -> Vec<(ActionFactory, crate::intent::ActionRisk)> {
        ACTION_POOL
            .iter()
            .map(|f| (*f, crate::exploration::risk::classify_action(&(f.1)())))
            .filter(|(_, r)| *r <= self.allowed_risk)
            .collect()
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
    run_evidenced(session, seed, budget, recording_path, None)
}

/// Ledger-evidenced variant (audit finding 22): when `run_ctx` is given,
/// every executed action ALSO enters the run's canonical transaction
/// ledger — one history, not two. `graph` lives inside the caller's
/// `RunContext`, so the merge stays the caller's concern.
#[allow(clippy::too_many_arguments)]
pub fn run_evidenced(
    session: &mut Session,
    seed: u64,
    budget: Budget,
    recording_path: Option<&Path>,
    mut run_ctx: Option<&mut crate::run::RunContext>,
) -> anyhow::Result<ExploreReport> {
    let started = Instant::now();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut identities: Vec<crate::exploration::state_graph::StateIdentity> = Vec::new();
    let mut steps: Vec<ExplorationStep> = Vec::new();
    let mut exits = Vec::new();
    let mut novel = 0u32;
    let mut last_id: Option<crate::exploration::state_graph::StateId> = None;
    let mut relaunches = 0u32;
    let mut terminated_early = false;
    // Item 29: multi-dimensional novelty scoreboard, fed after every step.
    let mut novelty_ledger = crate::exploration::novelty::NoveltyLedger::new();

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
        if identities.len() as u32 >= budget.max_unique_states {
            reason = ExplorationCompletionReason::UniqueStateBudget;
            break;
        }
        if steps.len() as u32 >= budget.max_depth {
            reason = ExplorationCompletionReason::DepthBudget;
            break;
        }

        // Risk-gated pool (items 27/28): actions above the budget's
        // allowance were excluded before the loop; the draw is uniform over
        // what the driver may actually execute.
        let admitted: Vec<ActionFactory> = ACTION_POOL
            .iter()
            .copied()
            .filter(|f| crate::exploration::risk::classify_action(&(f.1)()) <= budget.allowed_risk)
            .collect();
        let (name, mk) = admitted.choose(&mut rng).unwrap();
        // Go through the canonical executor so baseline-before-send and
        // anchored settle-wait ordering match MCP `tui_act` exactly
        // (re-review P0 "one canonical executor").
        let action = mk();
        let tx = match crate::execution::execute_act_as(
            session,
            crate::execution::DriveOrigin::Explore,
            &action,
            120,
            1500,
            false,
        ) {
            Ok(tx) => tx,
            Err(_e) => {
                exits.push(ProcessExit {
                    action_index: i,
                    action_name: format!("{name}:{}", action.signature()),
                    classification: ExitClassification::Unknown,
                    exit_code: None,
                    exit_signal: None,
                });
                terminated_early = true;
                reason = ExplorationCompletionReason::Failure;
                break;
            }
        };

        let settled = tx.settle;
        let after = tx.after().clone();
        let elapsed_ms = tx.elapsed_ms;
        // Audit finding 23: the step's identity is the exact canonical
        // signature, not the pool's generic kind name — `key` and `mouse_click`
        // collapse distinct actions; `mouse:left:click@12,3` does not.
        // Finding 22: the transaction also enters the run ledger when a sink
        // is present, linked as exploration evidence.
        let action_sig = action.signature();
        if let Some(run) = run_ctx.as_deref_mut() {
            let _ = run.record_interaction(&session.id, &tx);
        }

        // Layered identity for both frames (re-review P0 fix 3): semantic
        // analysis runs per step so interaction state (focus/selection)
        // participates in the identity — the same `analyze` the MCP observe
        // path uses. This is what stops the graph from collapsing
        // "same text, focus=Save" and "same text, focus=Cancel".
        //
        // Re-review P0.8: the analysis is the session's FUSED one, so a
        // cooperative app's native semantics participate in state identity
        // — never a bare re-inference that discards what the app reported.
        let before_identity = fused_identity_of(session, tx.before());
        let after_identity = fused_identity_of(session, &after);

        // Item 29: feed the novelty ledger with every dimension the step
        // can evidence. Failure to read a dimension (no native coverage,
        // no raw ring) shrinks the signal honestly rather than faking it.
        {
            let after_sem = session.fuse_screen(&after);
            let control_ids: Vec<String> =
                after_sem.controls.iter().map(|c| c.id.clone()).collect();
            let focus_edges: Vec<(String, String, String)> = [(
                tx.focus_before.as_ref().and_then(|f| f.0.clone()),
                tx.focus_after.as_ref().and_then(|f| f.0.clone()),
                Some(action.signature()),
            )]
            .into_iter()
            .filter_map(|(f, t, via)| match (f, t) {
                (Some(f), Some(t)) => Some((f, t, via.unwrap_or_default())),
                _ => None,
            })
            .collect();
            let coverage_targets: Vec<String> =
                session.native_coverage_targets().into_iter().collect();
            let mode_states = current_mode_states_pub(session);
            novelty_ledger.note(
                Some(&after_identity),
                &focus_edges,
                &control_ids,
                &coverage_targets,
                &mode_states,
            );
        }

        let novel_state = !identities.contains(&after_identity);
        if novel_state {
            identities.push(after_identity.clone());
        }
        if last_id.as_ref() != Some(&after_identity.id()) {
            novel += 1;
            last_id = Some(after_identity.id());
        }

        // Ordered step record — emitted while the action happened (item 12).
        steps.push(ExplorationStep {
            seq: i as u64,
            action: action_sig.clone(),
            before: before_identity,
            after: after_identity,
            changed: tx.before().structure_hash != after.structure_hash,
            settle: settled,
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
                action_name: action_sig.clone(),
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
                    last_id = None;
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
        screens_seen: identities.len(),
        unique_state_identities: identities,
        steps,
        completion_reason: reason,
        relaunches,
        exits,
        novel_transitions: novel,
        novelty: novelty_ledger.summary(),
        terminated_early,
        elapsed_ms: started.elapsed().as_millis() as u64,
        recording_path: final_path,
        recording_events: event_count,
    })
}

/// Layered [`StateIdentity`] for one frame (P0 fix 3), computed through the
/// session's fused semantic authority (re-review P0.8): the same fused truth
/// every observe mode serves, so state identity built from a cooperative
/// app's native semantics never regresses to bare inference.
fn fused_identity_of(
    session: &Session,
    screen: &crate::screen::ScreenState,
) -> crate::exploration::state_graph::StateIdentity {
    let sem = session.fuse_screen(screen);
    crate::exploration::state_graph::StateIdentity::with_semantic(screen, &sem)
}

/// Item 29: the current tri-state terminal-mode map, folded from the raw
/// protocol timeline. Empty when the engine retains no raw bytes (the
/// novelty ledger loses the mode dimension honestly).
pub fn current_mode_states_pub(
    session: &mut Session,
) -> Vec<(String, crate::protocol::KnownModeState)> {
    let (bytes, cap, dropped) = match session.raw_output_window() {
        Ok(w) => w,
        // Audit P1-44: a failed read loses the mode dimension for this step
        // (same honest outcome as a zero-retention engine), with the cause
        // on stderr.
        Err(e) => {
            eprintln!("[exploration] raw output window read failed: {e}");
            return Vec::new();
        }
    };
    if cap == 0 {
        return Vec::new();
    }
    let trace = crate::protocol::ProtocolTrace::decode(&bytes);
    crate::protocol::fold_mode_states(&trace.modes, dropped == 0)
        .into_iter()
        .map(|(m, s)| (m.to_string(), s))
        .collect()
}

/// Feed executed steps into the run's state graph — from the ordered record
/// of what actually happened, keyed by layered identity, not by bare
/// structure hashes (item 12 + re-review P0 fix 3). Interaction-distinct
/// states (same text, different focus/selection) become separate nodes.
pub fn record_steps(
    graph: &mut crate::exploration::state_graph::StateGraph,
    steps: &[ExplorationStep],
) {
    for s in steps {
        graph.record_transition_identity(&s.before, &s.after, &s.action);
    }
}
