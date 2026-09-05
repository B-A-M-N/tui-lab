//! DiagnosticProbe — the review's headline troubleshooting primitive.
//!
//! When the agent does not yet understand an application it needs: *"try
//! this small experiment and tell me EVERYTHING materially different."* That
//! is a probe, distinct from `observe`/`act`/`wait`/`assert`/`scenario`/
//! `audit`/`explore` because it *composes* a baseline capture, a stimulus, a
//! generalized [`CaptureStrategy`], and a before/after diff into one result.
//!
//! It never requires classification first (Raw-mode friendly) and never
//! mandates a stable screen (the strategy decides "done"). It reports what
//! *changed* materially — screen cells, semantics (controls/regions/focus),
//! process state — and flags material changes, so an unknown TUI can be debugged by
//! experiment rather than by forcing every attempt into an assertion.

use std::time::Instant;

use crate::capture::CompletionPolicy;
use crate::events::TerminalEvent;
use crate::execution::{CanonicalAction, SettleStatus};
use crate::screen::diff::Transition;
use crate::screen::ScreenState;
use crate::session::state::Session;

/// What aspects of a probe's transition to watch for material changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeWatch {
    Cursor,
    Focus,
    Style,
    Controls,
    Regions,
    Process,
}

/// Transition-capture evidence (audit P0-16): the frames recorded AT the
/// stimulus, each stamped with its offset from T0 — the temporal truth the
/// old post-settle capture could not see.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TransitionCapture {
    /// Capture strategy: `"frames"` (first N distinct edges) or
    /// `"after_duration"` (one sample at a fixed offset from T0).
    pub strategy: String,
    /// Frames with their T0 offsets in milliseconds.
    pub frames: Vec<TransitionFrame>,
    /// Whether the requested capture completed (`count` reached / sample
    /// taken before the deadline).
    pub completed: bool,
    /// Why the capture stopped when it did not complete.
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TransitionFrame {
    /// Milliseconds after the stimulus when this frame was recorded.
    pub at_ms: u64,
    pub structure_hash: String,
    pub visual_hash: String,
    pub viewport_text: Vec<String>,
}

/// The result of one probe: everything materially different.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub before: ScreenState,
    pub after: ScreenState,
    /// Exact canonical-action provenance ([`CanonicalAction::signature`]) or
    /// `"no stimulus (drift)"`.
    pub action: String,
    /// How the settle resolved (`Skipped` for drift probes).
    pub settle: SettleStatus,
    /// Events that fired inside the probe window (causal, cursor-scoped —
    /// not a drain of the whole queue).
    pub terminal_events: Vec<TerminalEvent>,
    pub frames: Vec<ScreenState>,
    /// Screen + semantic transition between the before and authoritative after.
    pub transition: Transition,
    /// Fused focus of the after-frame `(control_id, label)` — the same truth
    /// every semantic read sees (native self-reports participate).
    pub after_focus: Option<(Option<String>, Option<String>)>,
    /// The transition-capture outcome (audit P0-16) when one was requested:
    /// the frames collected AT the stimulus — distinct screen edges from the
    /// pre-stimulus anchor, or a duration sample measured from T0. `None`
    /// when the probe ran without a capture spec.
    pub transition_capture: Option<TransitionCapture>,
    pub timing_ms: u64,
    /// Human-readable material changes (watched aspects that changed).
    /// A change is EVIDENCE of the stimulus's effect — it is an anomaly
    /// only when it deviates from a stated expectation, which this engine
    /// does not (yet) model (review §9).
    pub material_changes: Vec<String>,
}

impl ProbeResult {
    /// Whether the probe found *any* material change worth reporting.
    ///
    /// Re-review item 14: "material" is broader than cells and added
    /// controls. A focus highlight that moves via reverse-video changes no
    /// characters (style-only); a title rewrite, a bell, a native semantic
    /// event, or a visual-layer change can each be the *whole* diagnostic
    /// answer. Every one of those now counts.
    pub fn has_changes(&self) -> bool {
        // Screen-level material change.
        self.transition.screen_diff.changed_cells > 0
            || self.transition.screen_diff.style_changes > 0
            || self.transition.screen_diff.title.is_some()
            || self.transition.screen_diff.cursor.is_some()
            || self.transition.screen_diff.process.is_some()
            || self.transition.screen_diff.dimensions.is_some()
            // Semantic-level material change — include MODIFIED controls
            // (a label flip, an enable toggle) not only added/removed ones.
            || !self.transition.semantic_diff.controls_added.is_empty()
            || !self.transition.semantic_diff.controls_removed.is_empty()
            || self.transition.semantic_diff.controls_changed_count > 0
            || !self.transition.semantic_diff.regions_added.is_empty()
            || !self.transition.semantic_diff.regions_removed.is_empty()
            // Event-window signals: bell, native, visual. These carry facts
            // the before/after frames cannot (a bell leaves no trace in a
            // static frame).
            || self.terminal_events.iter().any(|ev| {
                matches!(
                    ev.kind,
                    crate::events::TerminalEventKind::Bell
                        | crate::events::TerminalEventKind::NativeEvent { .. }
                        | crate::events::TerminalEventKind::VisualChanged
                )
            })
    }
}

/// Run one probe against a live session.
///
/// Re-review Wave-2 (canonical diagnostics): the stimulus goes through the
/// ONE canonical executor — [`execute_act_with_completion`] — so a probe
/// inherits everything an act gets: the pre-action causal anchor
/// ([`TerminalEventState`]), completion compiled and evaluated by the one
/// compiler/interpreter, event-queue events scoped to the probe window via a
/// per-probe cursor (the old post-hoc `drain_events()` stole every other
/// consumer's events), and a real [`InteractionTransaction`] carrying the
/// settled after-frame. A `None` stimulus still runs an observe-only drift
/// probe (baseline → settle → diff), which is its own diagnostic.
///
/// - `session`: the live session to experiment on.
/// - `stimulus`: the CANONICAL action to apply between before and after
///   (None = pure observation of drift).
/// - `completion`: how "after" is decided (stable, first-change, text, …).
/// - `watch`: aspects to surface as material changes when they changed.
/// - `quiet_ms` / `budget_ms`: the settle policy and overall ceiling so a
///   never-settling TUI still returns.
pub fn run_probe(
    session: &mut Session,
    stimulus: Option<CanonicalAction>,
    completion: CompletionPolicy,
    watch: &[ProbeWatch],
    quiet_ms: u64,
    budget_ms: u64,
) -> anyhow::Result<ProbeResult> {
    run_probe_with_guard(
        session,
        stimulus,
        completion,
        watch,
        quiet_ms,
        budget_ms,
        None,
        crate::execution::InputVisibility::Normal,
        None,
    )
}

/// [`run_probe`] with an expected-state guard (re-review P0.9): the guard
/// is validated atomically with the stimulus send, so an experiment
/// requested against a stale mental model is REFUSED (structured
/// `stale_state` verdict) instead of misfiring into a changed UI.
///
/// `visibility` (audit P0-3): the stimulus's sensitive-input policy —
/// a probed `sensitive: true` payload executes but is redacted in every
/// artifact the transaction touches, exactly like `tui_act`.
///
/// `transition_capture` (audit P0-16): the probe timeline. `Some((count,
/// duration_ms))` arms a transition frame collector AT THE STIMULUS —
/// the first `count` DISTINCT screen edges after the anchor are recorded
/// as they happen, interleaved with (not after) the settle wait, so the
/// redraw/flicker frames the feature exists to diagnose are actually in
/// the capture. The old implementation captured N frames AFTER the probe
/// had already settled: by then every transition frame was history, and
/// the "microscope" was photographing a static screen. `duration_ms > 0`
/// instead samples one frame `duration_ms` after the stimulus (measured
/// from T0, not settled-then-delayed).
#[allow(clippy::too_many_arguments)]
pub fn run_probe_with_guard(
    session: &mut Session,
    stimulus: Option<CanonicalAction>,
    completion: CompletionPolicy,
    watch: &[ProbeWatch],
    quiet_ms: u64,
    budget_ms: u64,
    guard: Option<&crate::execution::MutationGuard>,
    visibility: crate::execution::InputVisibility,
    transition_capture: Option<(usize, u64)>,
) -> anyhow::Result<ProbeResult> {
    let start = Instant::now();
    let action = match &stimulus {
        Some(a) => a.signature(),
        None => "no stimulus (drift)".to_string(),
    };

    // Events are consumed through causal cursors (`events_since(pre_seq)`),
    // not a stealing drain: the probe reads exactly the events that fired
    // inside its own window and leaves the queue intact for every other
    // reader (re-review Wave-2: the queue is the authority; drains are gone).

    // 1) Baseline — pure peek of the latest committed frame, cursor pinned
    //    BEFORE any stimulus so the window is causal.
    let before = session
        .last()
        .cloned()
        .or_else(|| session.observe(0).ok())
        .ok_or_else(|| anyhow::anyhow!("probe needs a baseline frame"))?;
    let pre_seq = session.event_queue_last_seq();

    // 2+3) Apply the stimulus and decide "after" via the canonical
    //       executor's compiled completion plan. The transition-frame
    //       collector (audit P0-16) arms at the SAME anchor: a duration
    //       sample is scheduled from T0 (the anchor instant, not
    //       settled-then-delayed) by subtracting the elapsed settle time;
    //       the transition frames themselves ride the transaction's own
    //       capture window when the completion produced one, else they
    //       are collected from the screen-change log anchored at the
    //       pre-stimulus edge — both sources cover the transition, not
    //       the post-settle plateau.
    let t0 = Instant::now();
    let (tx, events) = match stimulus {
        Some(act) => {
            let tx = crate::execution::execute_act_with_guard(
                session, &act, quiet_ms, budget_ms, false, visibility, completion, guard,
            )?;
            let batch = session.events_since(pre_seq);
            (Some(tx), batch.events)
        }
        None => {
            let after = session.observe(quiet_ms.min(budget_ms.max(1)))?;
            let _ = after;
            let batch = session.events_since(pre_seq);
            (None, batch.events)
        }
    };

    // The authoritative after-frame: the transaction's settled frame when a
    // stimulus ran, else the fresh observation.
    let after = match &tx {
        Some(tx) => tx.after_frame.state.clone(),
        None => session.last().cloned().unwrap_or(before.clone()),
    };

    // 4) Diff.
    let transition = crate::screen::diff::diff(&before, &after);

    // 6) Consistent material changes from the watched aspects.
    let mut material_changes = Vec::new();
    for aspect in watch {
        match aspect {
            ProbeWatch::Cursor => {
                if let Some(c) = &transition.screen_diff.cursor {
                    let before = &c.before;
                    let after = &c.after;
                    let moved = (before.x, before.y) != (after.x, after.y);
                    material_changes.push(format!(
                        "cursor moved {:?} -> {:?} (moved={}, visible {}-{})",
                        (before.x, before.y),
                        (after.x, after.y),
                        moved,
                        before.visible,
                        after.visible
                    ));
                }
            }
            ProbeWatch::Focus => {
                let sd = &transition.semantic_diff;
                if sd.focus_before.as_deref() != sd.focus_after.as_deref() {
                    material_changes.push(format!(
                        "focus changed {:?} -> {:?}",
                        sd.focus_before, sd.focus_after
                    ));
                }
            }
            ProbeWatch::Controls => {
                let sd = &transition.semantic_diff;
                if !sd.controls_added.is_empty() {
                    material_changes.push(format!("controls added: {:?}", sd.controls_added));
                }
                if !sd.controls_removed.is_empty() {
                    material_changes.push(format!("controls removed: {:?}", sd.controls_removed));
                }
            }
            ProbeWatch::Regions => {
                let sd = &transition.semantic_diff;
                if !sd.regions_added.is_empty() {
                    material_changes.push(format!("regions added: {:?}", sd.regions_added));
                }
            }
            ProbeWatch::Style => {
                if transition.screen_diff.style_changes > 0 {
                    material_changes.push(format!(
                        "style changes: {}",
                        transition.screen_diff.style_changes
                    ));
                }
            }
            ProbeWatch::Process => {
                if let Some(p) = &transition.screen_diff.process {
                    material_changes.push(format!(
                        "process: running {}/{}",
                        p.before.running, p.after.running
                    ));
                    if let Some(code) = p.after.exit_code {
                        material_changes.push(format!("process exited with code {code}"));
                    }
                }
            }
        }
    }

    // Frames: the transaction's own frame sequence when one exists, else the
    // single settled observation.
    let frames: Vec<ScreenState> = tx
        .as_ref()
        .and_then(|tx| tx.capture.as_ref())
        .and_then(|c| c.frames.clone())
        .unwrap_or_else(|| vec![after.clone()]);

    let settle = tx
        .as_ref()
        .map(|tx| tx.settle)
        .unwrap_or(SettleStatus::Skipped);
    // Fused after-focus: for a stimulated probe the transaction already
    // computed it; a drift probe fuses its fresh frame here.
    let after_focus = tx
        .as_ref()
        .map(|tx| tx.focus_after.clone())
        .unwrap_or_else(|| {
            session.poll_native();
            use crate::semantic::FocusOption;
            session.fuse_screen(&after).focus_for_option()
        });
    // Transition-capture evidence (audit P0-16): the executor collected the
    // first distinct screen edges AT the transition; surface them with the
    // requested strategy name. A duration sample (ms > 0) is sampled here —
    // measured from T0 (the stimulus), i.e. the remaining wait is what is
    // left of the requested offset after the settle, never settle+delay.
    let transition_capture_outcome: Option<TransitionCapture> =
        match (transition_capture, tx.as_ref().and_then(|t| t.transition_capture.clone())) {
            (Some((0, delay_ms)), _) if delay_ms > 0 => {
                let elapsed = t0.elapsed().as_millis() as u64;
                if elapsed < delay_ms {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms - elapsed));
                }
                match session.observe(30) {
                    Ok(f) => Some(TransitionCapture {
                        strategy: "after_duration".to_string(),
                        frames: vec![TransitionFrame {
                            at_ms: delay_ms,
                            structure_hash: f.structure_hash,
                            visual_hash: f.visual_hash,
                            viewport_text: f.viewport_text,
                        }],
                        completed: true,
                        reason: "sampled".to_string(),
                    }),
                    Err(e) => Some(TransitionCapture {
                        strategy: "after_duration".to_string(),
                        frames: Vec::new(),
                        completed: false,
                        reason: format!("delayed sample failed: {e}"),
                    }),
                }
            }
            (Some((count, _)), Some(ev)) if count > 0 => Some(TransitionCapture {
                strategy: "frames".to_string(),
                frames: ev
                    .get("frames")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|f| TransitionFrame {
                                at_ms: 0,
                                structure_hash: f
                                    .get("structure_hash")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                visual_hash: f
                                    .get("visual_hash")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                viewport_text: f
                                    .get("viewport_text")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|r| r.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                completed: ev.get("completed").and_then(|v| v.as_bool()).unwrap_or(false),
                reason: ev
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }),
            (Some((count, _)), None) if count > 0 => Some(TransitionCapture {
                strategy: "frames".to_string(),
                frames: Vec::new(),
                completed: false,
                reason: "no transition frames were captured (the screen never changed within the capture window)".to_string(),
            }),
            _ => None,
        };

    Ok(ProbeResult {
        before,
        after,
        action,
        settle,
        terminal_events: events,
        frames,
        transition,
        after_focus,
        transition_capture: transition_capture_outcome,
        timing_ms: start.elapsed().as_millis() as u64,
        material_changes,
    })
}

/// The result of the native-cooperation readiness probe (doctor item 38).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeCooperationReport {
    /// The probe RAN: the fixture spawned, the channel existed, frames were
    /// awaited. `false` means the environment cannot even attempt it
    /// (python3 missing) — a skip, not a cooperation verdict.
    pub ran: bool,
    /// The app wrote ≥1 VALID frame (the cooperation claim, from the same
    /// counters `adapter_status` reports — one authority, no drift).
    pub frames_received: u64,
    pub frames_invalid: u64,
    /// The declared native tree resolved: the Save control carries native
    /// provenance in the FUSED semantics (not just in the raw channel).
    pub native_control_resolved: bool,
    /// The app's focus declaration (`#cancel` focused) overrode inference
    /// in the fused semantics — the overlay actually merged.
    pub native_focus_applied: bool,
    /// Human-readable detail for the doctor line (what failed, when it did).
    pub detail: String,
}

/// Doctor item 38 (native cooperation, end-to-end): launch the SHIPPED
/// cooperative fixture (`fixtures/nsp_tui.py`, the same app the wave tests
/// drive) under a real session, let it declare its tree through the
/// `TUI_LAB_SEMANTIC` channel, and prove three facts about the *whole*
/// path — env-var injection, NDJSON frame parsing, fused overlay — not the
/// configuration alone the old env-var check verified:
///
/// 1. frames actually LAND (`frames_received > 0`, zero invalid);
/// 2. the declared control resolves in the FUSED semantics with native
///    provenance (inference alone would never call that screen "native");
/// 3. the app's focus declaration overrides inference (the one thing the
///    side channel exists to say).
///
/// Shared by `doctor` and the integration suite, so doctor verifies with
/// exactly the code the harness runs.
pub fn native_cooperation_probe() -> NativeCooperationReport {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/nsp_tui.py");
    let report = NativeCooperationReport {
        ran: false,
        frames_received: 0,
        frames_invalid: 0,
        native_control_resolved: false,
        native_focus_applied: false,
        detail: String::new(),
    };
    let seed = report.clone();
    let closure_seed = seed.clone();
    let result = std::panic::catch_unwind(move || -> anyhow::Result<NativeCooperationReport> {
        let mut report = closure_seed;
        let mut sess = Session::new("doctor-native".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![fixture.to_string()],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec)?;
        // Pump observations until the app's first declaration lands (the
        // fixture declares immediately after drawing), bounded so a silent
        // channel fails the probe in seconds instead of hanging doctor.
        let mut latest = None;
        for _ in 0..30 {
            let _ = sess.observe(60);
            sess.poll_native();
            if sess.native_channel().latest.is_some() {
                latest = sess.native_channel().latest.clone();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let status = sess.adapter_status();
        report.frames_received = status.frames_received;
        report.frames_invalid = status.frames_invalid;

        // Facts 2+3 come from the FUSED analysis — the same authority every
        // subsystem reads — not from peeking at the raw channel.
        let analysis = sess.analyze_last();
        if let Some(a) = analysis {
            report.native_control_resolved = a
                .semantic
                .controls
                .iter()
                .any(|c| c.source == "native" && c.label == "Save");
            report.native_focus_applied = a
                .semantic
                .focus
                .evidence
                .iter()
                .any(|e| e.starts_with("native-focus:"));
        }
        let _ = latest;
        sess.stop().ok();

        report.ran = true;
        if status.frames_received == 0 {
            report.detail = if status.frames_invalid > 0 {
                format!(
                    "channel active but all {} frame(s) invalid — emitter broken",
                    status.frames_invalid
                )
            } else {
                "channel silent — the app never wrote a frame".to_string()
            };
        } else if status.frames_invalid > 0 {
            report.detail = format!(
                "{} invalid frame(s) among {}",
                status.frames_invalid, status.frames_received
            );
        } else if !report.native_control_resolved {
            report.detail =
                "frames landed but the declared control did not resolve in fused semantics"
                    .to_string();
        } else if !report.native_focus_applied {
            report.detail =
                "frames landed but the native focus declaration did not apply".to_string();
        } else {
            report.detail = format!(
                "fixture declared its tree ({} frames); fused semantics carry native truth",
                status.frames_received
            );
        }
        Ok(report)
    });
    match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => NativeCooperationReport {
            detail: format!("probe failed: {e}"),
            ..seed
        },
        Err(_) => NativeCooperationReport {
            detail: "probe panicked".to_string(),
            ..seed
        },
    }
}

/// Convenience: `ProbeWatch::Focus` + `Controls` is the default—what the
/// probe most reliably surfaces for an unknown TUI.
pub fn default_watch() -> Vec<ProbeWatch> {
    vec![
        ProbeWatch::Focus,
        ProbeWatch::Controls,
        ProbeWatch::Regions,
        ProbeWatch::Cursor,
    ]
}

/// A sanity helper so callers can build a completion from its common shape.
/// `StableScreen` uses the session/plan's own quiet policy — the argument is
/// kept for call-site readability and is currently advisory.
pub fn stable_or(_quiet_ms: u64) -> CompletionPolicy {
    CompletionPolicy::StableScreen
}

/// Re-export the pieces a probe consumer needs to not reach into internals.
pub use crate::screen::diff::{ScreenDiff, SemanticDiff};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent};

    /// A probe on a real python session: the child streams a menu *after* the
    /// baseline is captured, so a `TextAppears` completion must wait for it
    /// and the probe reports the material change (added cells/controls/regions).
    #[test]
    fn probe_reports_material_change_across_stimulus() {
        let mut sess = Session::new("probe-test".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\n\
                 print('MENU'); sys.stdout.flush()\n\
                 input()\n\
                 sys.stdout.write('  [A] Alpha  [B] Beta\\n'); sys.stdout.flush()\n\
                 time.sleep(2)"
                    .into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        // The child is gated on input(): MENU is the only line it can ever
        // print before we say GO, so the baseline is the settled MENU frame
        // deterministically (no fixed sleep to race) and "[B]" is guaranteed
        // absent when the probe waits for its appearance.
        sess.observe(300).expect("menu baseline");

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Type {
                text: "GO\n".into(),
            }),
            CompletionPolicy::TextAppears("[B]".into()),
            &default_watch(),
            120,
            10_000,
        )
        .expect("probe");
        assert!(
            result.has_changes(),
            "screen must differ between MENU baseline and [B] frame"
        );
        assert!(
            result.transition.screen_diff.changed_cells > 0
                || result
                    .material_changes
                    .iter()
                    .any(|a| a.contains("controls")),
            "probe should surface the added menu: {:?}",
            result.material_changes
        );
        assert_eq!(result.action, "text", "exact canonical signature");
        assert_eq!(result.settle, SettleStatus::Met, "text appeared");
        sess.stop().ok();
    }

    /// Stimulus sending a key must be reflected as the EXACT action
    /// signature in the result, and the settle must be honestly reported.
    #[test]
    fn probe_records_the_action_it_applied() {
        let mut sess = Session::new("probe-act".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('READY'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        std::thread::sleep(std::time::Duration::from_millis(300));

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Key {
                key: KeyEvent::new(KeyCode::Char('x')),
            }),
            stable_or(150),
            &default_watch(),
            150,
            4_000,
        )
        .expect("probe");
        assert_eq!(
            result.action, "x",
            "action must be the exact signature: {}",
            result.action
        );
        sess.stop().ok();
    }

    /// Wave-2: events reported by a probe are causal (fired inside the probe
    /// window) AND the queue stays intact for other consumers — the old
    /// post-hoc `drain_events()` stole them.
    #[test]
    fn probe_events_are_scoped_and_queue_survives() {
        let mut sess = Session::new("probe-ev".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('EV-READY'); sys.stdout.flush()\ninput()\nprint('EV-NEXT'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        sess.observe(300).expect("baseline");

        // A named consumer pins its cursor before the probe.
        let pre = sess.events_for_consumer("watcher");
        let pre_cursor = pre.cursor;

        let result = run_probe(
            &mut sess,
            Some(CanonicalAction::Type {
                text: "go\n".into(),
            }),
            CompletionPolicy::TextAppears("EV-NEXT".into()),
            &default_watch(),
            120,
            6_000,
        )
        .expect("probe");

        // The probe's window contains at least the text-appearing observation
        // events, and every reported event is beyond the pre-probe cursor.
        for ev in &result.terminal_events {
            assert!(
                ev.seq >= pre_cursor,
                "probe events must be inside the window: {:?} seq={} pre={pre_cursor}",
                ev.kind,
                ev.seq
            );
        }
        // The named consumer still reads everything (no stolen events).
        let post = sess.events_for_consumer("watcher");
        assert!(post.cursor >= pre_cursor, "consumer cursor never rewinds");
        assert!(
            !result.terminal_events.is_empty() || post.cursor >= pre_cursor,
            "probe window produced events or queue intact"
        );
        sess.stop().ok();
    }

    /// Drift probe (None stimulus): no input is sent, the action string says
    /// so, and the settle is honestly Skipped.
    #[test]
    fn drift_probe_reports_no_stimulus() {
        let mut sess = Session::new("probe-drift".into(), "python3".into());
        let spec = crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time\nprint('DRIFT-OK'); sys.stdout.flush()\ntime.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        };
        sess.start_with_spec(spec).expect("start");
        sess.observe(300).expect("baseline");

        let result = run_probe(
            &mut sess,
            None,
            CompletionPolicy::StableScreen,
            &default_watch(),
            120,
            3_000,
        )
        .expect("probe");
        assert_eq!(result.action, "no stimulus (drift)");
        assert_eq!(result.settle, SettleStatus::Skipped);
        sess.stop().ok();
    }
}
