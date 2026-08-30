//! Seeded deterministic random exploration (spec section 4.2). Same app + same
//! seed => same action sequence when practical. Records everything for replay.
//!
//! Fixes per audit:
//!   * Item 43: classify clean_exit vs crash vs signal_exit vs backend_failure
//!   * Item 44: track actual executed actions (not requested count)
//!   * Item 41: bounded exploration with budget
//!   * Item 42: preserve exact action traces and optional recording

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::path::Path;

use crate::backend::{Input, KeyEvent, KeyModifiers};
use crate::screen::diff;
use crate::session::state::Session;

#[derive(Debug, serde::Serialize)]
pub struct ExploreReport {
    pub seed: u64,
    pub actions_run: u32,
    pub actions_requested: u32,
    pub screens_seen: usize,
    pub structure_hashes: Vec<String>,
    pub exits: Vec<ProcessExit>,
    pub novel_transitions: u32,
    pub terminated_early: bool,
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

fn key(code: crate::backend::KeyCode) -> Input {
    Input::Key(KeyEvent::new(code))
}

/// An action factory: name + zero-arg input constructor.
type ActionFactory = (&'static str, fn() -> Input);

const ACTION_POOL: &[ActionFactory] = &[
    ("tab", || key(crate::backend::KeyCode::Tab)),
    ("shift+tab", || {
        Input::Key(KeyEvent::with_modifiers(
            crate::backend::KeyCode::Tab,
            KeyModifiers::SHIFT,
        ))
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

/// Run a seeded random exploration. Returns a report with evidence.
pub fn run(
    session: &mut Session,
    seed: u64,
    actions: u32,
    recording_path: Option<&Path>,
) -> anyhow::Result<ExploreReport> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut hashes = Vec::new();
    let mut exits = Vec::new();
    let mut novel = 0u32;
    let mut last_hash: Option<String> = None;
    let mut actions_executed = 0u32;
    let mut terminated_early = false;

    for i in 0..actions {
        let (name, mk) = ACTION_POOL.choose(&mut rng).unwrap();
        let before = session.observe(30)?;
        if let Err(_e) = session.send(mk()) {
            exits.push(ProcessExit {
                action_index: i,
                action_name: name.to_string(),
                classification: ExitClassification::Unknown,
                exit_code: None,
                exit_signal: None,
            });
            terminated_early = true;
            break;
        }
        actions_executed += 1;

        // wait for settle
        let _ = session.wait(
            crate::backend::WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(120),
                after_screen_seq: None,
            },
            120,
        );
        let after = session.observe(50)?;
        let tr = diff(&before, &after);
        if last_hash.as_deref() != Some(&after.structure_hash) {
            novel += 1;
            last_hash = Some(after.structure_hash.clone());
        }
        if !hashes.contains(&after.structure_hash) {
            hashes.push(after.structure_hash.clone());
        }

        // Classify process exit properly (item 43)
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

            // relaunch for remaining budget when possible
            let spec = session.launch().cloned().unwrap_or_else(|| {
                crate::session::state::LaunchSpec::new(&session.command, before.cols, before.rows)
            });
            if let Err(_e) = session.start_with_spec(spec) {
                terminated_early = true;
                break;
            }
        }
        let _ = tr;
    }

    // Write recording if requested
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
        actions_run: actions_executed,
        actions_requested: actions,
        screens_seen: hashes.len(),
        structure_hashes: hashes,
        exits,
        novel_transitions: novel,
        terminated_early,
        recording_path: final_path,
        recording_events: event_count,
    })
}
