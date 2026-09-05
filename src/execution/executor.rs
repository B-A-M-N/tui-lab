//! The one canonical executor: [`execute_act`] and its guarded/
//! visibility/completion variants, plus the single completion-plan
//! interpreter (`run_completion_plan`) and fused focus extraction.
//!
//! Split from the former single-file `execution` (review §15 god-object
//! residue): every caller (MCP `tui_act`, ScenarioRunner, explorer, audit
//! drivers) routes through here, so there is exactly one interpretation of
//! settle semantics.

use crate::backend::{CanonicalFrame, CaptureOutcome, WaitCond};
use crate::capture::CompletionPolicy;
use crate::session::state::Session;
use rmcp::serde_json::json;

use super::guard::MutationGuard;
use super::record::{CanonicalAction, InputVisibility, ObservationAnchor, SettleStatus};
use super::transaction::ActionEnvelope;
use super::transaction::{build_render_transaction, ActTransactionGuard, InteractionTransaction};

impl<'a> Drop for ActTransactionGuard<'a> {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn execute_act(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
) -> Result<InteractionTransaction, anyhow::Error> {
    execute_act_with_completion(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        InputVisibility::Normal,
        CompletionPolicy::StableScreen,
    )
}

/// [`execute_act`] with an explicit [`InputVisibility`] policy. Sensitive
/// visibility routes the send through [`Session::send_unrecorded`] and marks
/// the transaction so every downstream recorder redacts. The completion
/// policy is the ordinary "action ⇒ stable screen" case.
pub fn execute_act_with_visibility(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
) -> Result<InteractionTransaction, anyhow::Error> {
    execute_act_with_completion(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        visibility,
        CompletionPolicy::StableScreen,
    )
}

/// The canonical act executor with a declared [`CompletionPolicy`] (review
/// P0: "the runtime still assumes action success usually means a stable
/// screen change"). Every action that is *not* "send then screen settles" —
/// copy-to-clipboard (silent), quit (process exit), "type until text
/// appears", a bell-only notify — historically produced a false
/// `settled=false` because the settle wait demanded screen quiet that never
/// came. A `completion` policy tells the executor what "done" actually means
/// for THIS action, so a silent/toggle/exit action is correctly reported as
/// `Met` rather than a spurious timeout.
pub fn execute_act_with_completion(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
    completion: CompletionPolicy,
) -> Result<InteractionTransaction, anyhow::Error> {
    execute_act_with_guard(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        visibility,
        completion,
        None,
    )
}

/// The full-form act executor with an optional [`MutationGuard`]
/// (re-review P0.9): when a guard is present, the executor validates the
/// caller's expected state IMMEDIATELY before the send — inside the same
/// call, so no other operation can interleave — and refuses to act on
/// drift. The returned error carries the structured `stale_state` payload
/// (category, expected, actual, summary) via its `stale_state` string; the
/// MCP layer surfaces it as an `ExecutionGuard` error, not a timeout.
///
/// This closes the observe→act race: an agent that read the screen, had a
/// modal open underneath it, then sent a click at the old coordinates now
/// gets an honest refusal instead of a silent misdirected input.
#[allow(clippy::too_many_arguments)]
pub fn execute_act_with_guard(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
    completion: CompletionPolicy,
    guard: Option<&MutationGuard>,
) -> Result<InteractionTransaction, anyhow::Error> {
    // Guard validation happens FIRST — before the baseline capture, before
    // anything else touches the session — so the checked state is the state
    // the action lands in.
    if let Some(g) = guard {
        if let Err(stale) = g.validate(session) {
            return Err(anyhow::anyhow!(
                "stale_state: {}",
                serde_json::to_string(&stale).unwrap_or_default()
            ));
        }
    }
    execute_act_inner(
        session,
        action,
        quiet_ms,
        settle_budget_ms,
        no_wait,
        visibility,
        completion,
        None,
    )
}

/// The body of the canonical executor (was `execute_act_with_completion`).
#[allow(clippy::too_many_arguments)]
fn execute_act_inner(
    session: &mut Session,
    action: &CanonicalAction,
    quiet_ms: u64,
    settle_budget_ms: u64,
    no_wait: bool,
    visibility: InputVisibility,
    completion: CompletionPolicy,
    transition_capture_frames: Option<usize>,
) -> Result<InteractionTransaction, anyhow::Error> {
    // Transition-capture evidence lands on the transaction (audit P0-16).
    let mut transition_frames_evidence: Option<serde_json::Value> = None;
    let before = match session.last().cloned() {
        Some(s) => s,
        None => session.observe(0)?,
    };
    let baseline = session.event_state();
    // Pre-action event-queue cursor, captured BEFORE the send. Used by
    // CompletionPolicy::Event to anchor on events that fire *after* this
    // action — the old code read the queue *after* the send and could miss
    // an immediately-firing event (race).
    let pre_event_seq = session.event_queue_last_seq();
    let anchor = ObservationAnchor {
        state: baseline,
        label: None,
        // Real monotonic per-session anchor index (re-review P1 fix 9).
        index: session.next_anchor(),
    };
    // Pre-action fused semantic identity + focus, computed BEFORE the
    // transaction guard so fused_frame(&self) can borrow_mut the cache
    // without conflicts. The String is stored so the guard window doesn't
    // hold it. Focus MUST be pinned here (re-review P0): the native
    // channel's snapshot is LIVE state, and by settle time it describes
    // the after-frame — fusing the before-frame against it then would read
    // post-action focus on both sides and cancel every transition.
    let (before_fused_identity, focus_before) = session
        .fused_frame()
        .map(|(sem, tree, _report)| {
            let focus = if sem.focus.control_id.is_none() && sem.focus.control.is_none() {
                None
            } else {
                Some((sem.focus.control_id.clone(), sem.focus.control.clone()))
            };
            (crate::semantic::semantic_identity_fused(&sem, &tree), focus)
        })
        .unwrap_or((Default::default(), None));
    // The ONE compiler runs on the pre-action frame before it moves into
    // the transaction evidence (re-review P0).
    let plan = crate::capture::compile_completion(
        &completion,
        &baseline,
        &before,
        pre_event_seq,
        quiet_ms,
        Some(before_fused_identity.clone()),
    );
    let mut before_frame = CanonicalFrame::new(before, 0, baseline.output_seq);
    before_frame.session_id = Some(session.id.clone());
    before_frame.generation = Some(session.generation);
    // Carry the FUSED identity established at capture time (review P0.5):
    // evidence persistence serializes truth, never recomputes it. `None`
    // here (an unfused capture) is a valid signal to fall back to the bare
    // grid identity at commit time.
    if !before_fused_identity.is_empty() {
        before_frame.semantic_identity = Some(before_fused_identity);
    }

    // Sensitive payloads bypass the recording hook for the WHOLE transaction
    // window (leak fix): the send is gated on input, and the settle wait's
    // captures are gated on output because the tty line discipline echoes
    // typed bytes back — the echo IS the payload. The PTY still receives
    // everything; only the cast is blind during the window.
    //
    // `ActTransactionGuard` owns the window as the transaction boundary: the
    // send and its settle wait run inside it, and whichever way the function
    // exits — a successful `commit`, a send error, or a settle-wait `Err` —
    // the guard restores recording. A failed mutation never leaves the session
    // permanently blind (Wave G review P1 18).
    let sensitive_window = matches!(
        visibility,
        InputVisibility::Sensitive | InputVisibility::NeverPersist
    );
    let mut window = ActTransactionGuard::begin(session, sensitive_window);
    let send_start = std::time::Instant::now();
    // Causal render bracket (re-review item 19): the absolute output-stream
    // offset at send time — everything the child emits from here to the
    // matching offset after settle IS this action's protocol response.
    let protocol_offset_before = window.sess().raw_window_range().1;
    let sent_at_unix_ms = crate::events::unix_ms();
    // Canonical mutation dispatch (re-review P0): Resize is a SESSION
    // mutation, not a byte write — it must go through Session::resize() so
    // the stored LaunchSpec follows the real viewport and the Session resize
    // event fires. Routing it through send()/Input::Resize silently left
    // launch.cols/rows stale, so a restart resurrected the old size.
    // Everything else is a transport write.
    let send_result = match action {
        CanonicalAction::Resize { cols, rows } => window.sess().resize(*cols, *rows),
        _ => window.sess().send(action.to_input()),
    };
    let send_ms = send_start.elapsed().as_millis() as u64;
    let send_failed = send_result.is_err();
    if send_failed {
        send_result?;
    }

    // A declared `NoWait` completion is equivalent to the transport's
    // no_wait flag: act, capture a fresh frame, report settlement as Skipped
    // (never a fake "settled").
    let skip = no_wait || matches!(completion, CompletionPolicy::NoWait);
    let settle_start = std::time::Instant::now();
    let (settle, elapsed_ms, after_state, after_screen_seq, capture) = if skip {
        // Even with no_wait, capture a fresh frame so callers always get
        // both before and after — but settlement was NOT tested. Reporting
        // `SettleStatus::Skipped` is the honest answer (re-review P1 fix 8).
        let s = window.sess().observe(quiet_ms)?;
        (SettleStatus::Skipped, 0, s, None, None)
    } else {
        let budget = settle_budget_ms.max(quiet_ms.saturating_add(1000));
        // Transition capture (audit P0-16): when armed, the FIRST distinct
        // screen edges after the send are collected HERE — at the
        // transition, before the settle wait — so the redraw/flicker frames
        // the capture exists to diagnose are in the evidence. Each edge
        // wait resolves quickly (quiet_for: 0), then the completion plan
        // independently decides the settled after-frame.
        if let Some(count) = transition_capture_frames {
            let anchor_seq = baseline.screen_seq;
            let t0 = std::time::Instant::now();
            let outcome = crate::capture::capture_frame_sequence(
                window.sess().backend_mut(),
                count,
                anchor_seq,
                std::time::Duration::from_millis(budget.min(2000)),
            );
            let frames_json = outcome
                .frames
                .iter()
                .map(|f| {
                    json!({
                        "structure_hash": f.structure_hash,
                        "visual_hash": f.visual_hash,
                        "viewport_text": f.viewport_text,
                    })
                })
                .collect::<Vec<_>>();
            transition_frames_evidence = Some(json!({
                "requested": outcome.requested,
                "captured": outcome.captured,
                "completed": outcome.completed,
                "reason": outcome.reason.name(),
                "elapsed_ms": outcome.elapsed_ms,
                "at_ms_from_send": t0.elapsed().as_millis() as u64,
                "frames": frames_json,
            }));
        }
        // The ONE compiler (re-review P0): every policy becomes a
        // CompletionPlan here; there is no second interpretation anywhere.
        let outcome = run_completion_plan(window.sess(), plan, &baseline, quiet_ms, budget)?;
        let capture = outcome;
        // `MayBeSilent` is special: the action may legitimately produce no
        // observable change (clipboard copy, an invisible toggle). Silence
        // here is a Met by construction — the plan reports it, never a
        // false `settled=false`.
        let settle = if capture.met {
            SettleStatus::Met
        } else {
            SettleStatus::TimedOut
        };
        let seq = capture.screen_seq;
        (
            settle,
            capture.elapsed_ms,
            capture.frame.clone(),
            Some(seq),
            Some(capture),
        )
    };
    let settle_ms = settle_start.elapsed().as_millis() as u64;
    // Input→first-byte latency (re-review item 26): the event queue records
    // Output events with unix-ms timestamps (fed by the reader thread), so
    // the first Output event at/after the pre-action cursor gives the
    // measured reaction time. `None` when the action produced no output.
    let first_byte_ms: Option<u64> = {
        let sess = window.sess();
        // Fold the reader thread's pending byte facts first — output that
        // arrived during the settle wait is still in `pending_ingest` until
        // the next observe() drains it; reading the queue without this
        // under-reports latency (None) even when bytes arrived.
        sess.absorb_ingest_now();
        sess.events_since(pre_event_seq)
            .events
            .iter()
            .find(|ev| matches!(ev.kind, crate::events::TerminalEventKind::Output { .. }))
            .map(|ev| ev.at.saturating_sub(sent_at_unix_ms))
    };
    let protocol_offset_after = window.sess().raw_window_range().1;

    // Item 26: first-frame and first-semantic latencies. The frame figure
    // comes from the backend's measured screen-change log (the settle path
    // never routes through Session::observe, so no ScreenChanged event
    // lands in the queue — the log is where the pump stamps the change).
    // The semantic figure stays event-driven: an explicit SemanticChanged /
    // FocusChanged event inside the settle window.
    let phase_latencies = {
        let sess = window.sess();
        sess.absorb_ingest_now();
        let pre_screen_seq = baseline.screen_seq;
        let frame = sess
            .screen_changes_since(pre_screen_seq)
            .first()
            .map(|(_, at_ms)| at_ms.saturating_sub(sent_at_unix_ms));
        let batch = sess.events_since(pre_event_seq);
        // Semantic change: an explicit SemanticChanged event, or a screen
        // change whose transition actually altered semantics (dirty rows
        // overlap a focus/structure delta is implicit; keep it honest: an
        // explicit event only).
        let semantic = batch
            .events
            .iter()
            .find(|ev| {
                matches!(
                    ev.kind,
                    crate::events::TerminalEventKind::SemanticChanged
                        | crate::events::TerminalEventKind::FocusChanged { .. }
                )
            })
            .map(|ev| ev.at.saturating_sub(sent_at_unix_ms));
        (frame, semantic)
    };

    let transition = crate::screen::diff(&before_frame.state, &after_state);
    let render = build_render_transaction(
        window.sess(),
        &action.signature(),
        protocol_offset_before,
        protocol_offset_after,
        first_byte_ms,
        phase_latencies.0,
        phase_latencies.1,
        &before_frame.state,
        &after_state,
    );
    let mut after_frame = CanonicalFrame::new(
        after_state,
        after_screen_seq.unwrap_or(0),
        capture.as_ref().map(|c| c.output_seq).unwrap_or(0),
    );
    after_frame.session_id = Some(window.sess().id.clone());
    after_frame.generation = Some(window.sess().generation);

    // Transaction-derived focus evidence (re-review P1): focus before was
    // pinned at anchor time (see the fused_frame call above — the native
    // snapshot is live state and cannot be replayed onto the before-frame
    // after the fact); focus after comes from the FUSED analysis of the
    // settled frame. Native self-reports participate on both sides — the
    // old bare `semantic::analyze` silently dropped native focus facts.
    // The run's focus graph consumes these in `record_interaction` —
    // summary/observe reads never create focus edges again. The drain is
    // required for the same reason: the settle's last observe can race the
    // app's post-action declare, and a non-polling read would fuse the
    // after-frame against the PRE-action native snapshot.
    let (focus_after, after_fused_identity);
    {
        let sess = window.sess();
        sess.poll_native();
        focus_after = frame_focus(sess, &after_frame.state);
        // FUSED identity of the settled frame (review P0.5): computed here
        // through the session (same cache + native overlay as every observe
        // mode) and attached to the frame so persist/commit never re-infer
        // bare truth from cells.
        after_fused_identity = sess
            .fuse_frame_full(&after_frame.state)
            .map(|(sem, tree)| crate::semantic::semantic_identity_fused(&sem, &tree))
            .unwrap_or_default();
    }
    if !after_fused_identity.is_empty() {
        after_frame.semantic_identity = Some(after_fused_identity);
    }

    // Sensitive window closed: the settled frame has been captured, so the
    // application's own (masked) rendering is recorded from here on. `commit`
    // restores recording and relinquishes the session.
    window.commit();

    Ok(InteractionTransaction {
        action: ActionEnvelope::new(action.clone(), visibility),
        anchor,
        before_frame,
        after_frame,
        settle,
        transition,
        capture,
        focus_before,
        focus_after,
        elapsed_ms,
        send_ms,
        settle_ms,
        render,
        transition_capture: transition_frames_evidence,
    })
}

/// Focus `(control_id, label)` from a frame via fused semantic analysis.
/// Native self-reports win when the app cooperates; otherwise style
/// inference — the same truth every observe mode sees.
fn frame_focus(
    session: &Session,
    screen: &crate::screen::ScreenState,
) -> Option<(Option<String>, Option<String>)> {
    // FUSED focus from the frame via the session's cache + native overlay:
    // the same truth every observe mode sees (re-review P0 — bare
    // `semantic::analyze` here silently dropped native focus facts).
    let sem = session.fuse_screen(screen);
    let f = sem.focus;
    if f.control_id.is_none() && f.control.is_none() {
        return None;
    }
    Some((f.control_id, f.control))
}

/// The ONE evaluator for compiled completion plans (re-review P0: exactly
/// one interpreter). `BackendWait`/`Bell` conditions run against the
/// backend's event-sequenced wait, anchored to the pre-action baseline.
/// Everything the backend cannot prove — real semantic change, arbitrary
/// event kinds, causally-anchored text transitions — is evaluated HERE,
/// above the backend layer, where the session's event queue and semantic
/// analysis live.
fn run_completion_plan(
    session: &mut Session,
    plan: crate::capture::CompletionPlan,
    anchor: &crate::backend::TerminalEventState,
    quiet_ms: u64,
    budget_ms: u64,
) -> anyhow::Result<CaptureOutcome> {
    use crate::capture::CompletionPlan as Plan;
    let budget = std::time::Duration::from_millis(budget_ms);
    match plan {
        // Backend-proven conditions: one `wait_after` call each.
        Plan::BackendWait(cond) => Ok(CaptureOutcome::from_wait(
            session.wait_after(*anchor, cond, budget_ms)?,
        )),
        Plan::Bell(after_bell_seq) => Ok(CaptureOutcome::from_wait(session.wait_after(
            *anchor,
            WaitCond::Bell {
                // The compile step already anchored this to the pre-action
                // bell counter; `wait_after` must not overwrite it.
                after_bell_seq: Some(after_bell_seq),
            },
            budget_ms,
        )?)),
        Plan::ProcessExit => Ok(CaptureOutcome::from_wait(session.wait_after(
            *anchor,
            WaitCond::ProcessExit,
            budget_ms,
        )?)),
        Plan::CommandDone => Ok(CaptureOutcome::from_wait(session.wait_after(
            *anchor,
            WaitCond::CommandDone {
                // Anchor at pre-action command_seq so "command #N finished"
                // is expressible even after several commands have run.
                after_command_seq: Some(anchor.command_seq),
            },
            budget_ms,
        )?)),
        // Any observable edge — screen, bell, title, cursor — beyond the
        // action anchor. Genuinely broader than a screen change (re-review
        // P0: bell-only / title-only reactions count).
        Plan::AnyActivity => Ok(CaptureOutcome::from_wait(session.wait_after(
            *anchor,
            WaitCond::AnyActivity {
                after_interaction_seq: Some(anchor.interaction_seq),
            },
            budget_ms,
        )?)),
        // The action may be silent: a SHORT grace window (not the whole
        // settle budget). A change inside the window is captured; silence
        // ends the wait as a successful completion with reason
        // `Idle`-proxied NoChange semantics (re-review P0 efficiency).
        Plan::SilentGrace(grace) => {
            let cond = WaitCond::AnyActivity {
                after_interaction_seq: Some(anchor.interaction_seq),
            };
            let outcome = session.wait_after(*anchor, cond, grace.as_millis() as u64)?;
            let mut out = CaptureOutcome::from_wait(outcome.clone());
            if !outcome.met {
                // No observable change in the grace window: the honest
                // reading of a silent action. Re-frame as success.
                out.frame = session.observe(quiet_ms)?;
                out.screen_seq = session.event_state().screen_seq;
                out.output_seq = session.event_state().output_seq;
                out.reason = crate::backend::CaptureReason::Idle;
                out.met = true;
            }
            Ok(out)
        }
        // Everything below is evaluated by polling the session's own
        // state — the backend has no primitive for these predicates.
        Plan::Event(matcher, pre_event_seq) => {
            let start = std::time::Instant::now();
            loop {
                // Session-queue events are synthesized inside observe()
                // (emit_frame_events diffs the fresh frame against the
                // previous one), so a wait that never observes never sees
                // new events — poll WITH a bounded idle window, not against
                // a static queue.
                let batch = session
                    .observe(quiet_ms.min(30))
                    .map(|_| session.events_since(pre_event_seq))?;
                for ev in &batch.events {
                    if matcher.matches(&ev.kind) {
                        let frame = session.observe(quiet_ms)?;
                        return Ok(CaptureOutcome {
                            reason: crate::backend::CaptureReason::ScreenChanged,
                            met: true,
                            screen_seq: session.event_state().screen_seq,
                            output_seq: session.event_state().output_seq,
                            frame,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            frames: None,
                        });
                    }
                }
                if start.elapsed() >= budget {
                    let frame = session.observe(quiet_ms)?;
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        // REAL semantic change (re-review P0): the fused semantic identity
        // must differ from the pre-action one. Drops the screen_seq gate
        // so native-only semantic changes (no pixels changed) can resolve.
        Plan::SemanticChange(before_identity) => {
            let start = std::time::Instant::now();
            loop {
                // Pump first: fused_frame() reads the LAST captured frame;
                // without an observe() the frame never advances and a change
                // that already happened is invisible to the identity check.
                let _ = session.observe(quiet_ms.min(30))?;
                if let Some((sem, tree, _report)) = session.fused_frame() {
                    if crate::semantic::semantic_identity_fused(&sem, &tree) != before_identity {
                        let frame = session.observe(quiet_ms)?;
                        return Ok(CaptureOutcome {
                            reason: crate::backend::CaptureReason::ScreenChanged,
                            met: true,
                            screen_seq: session.event_state().screen_seq,
                            output_seq: session.event_state().output_seq,
                            frame,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            frames: None,
                        });
                    }
                }
                if start.elapsed() >= budget {
                    let frame = session.observe(quiet_ms)?;
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        // Causally-anchored text transitions (re-review P0): `appears`
        // requires the text to be ABSENT at anchor and present after;
        // `disappears` the reverse. Presence at the anchor can never
        // satisfy an `appears` — the compiler recorded `was_present` and
        // the evaluator enforces the transition.
        Plan::TextAppears(text, was_present) => {
            let start = std::time::Instant::now();
            loop {
                let frame = session.observe(quiet_ms.min(30))?;
                let present = crate::capture::screen_contains(&frame, &text);
                if present && !was_present {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::TextMatched,
                        met: true,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                if start.elapsed() >= budget {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Plan::TextDisappears(text, was_present) => {
            let start = std::time::Instant::now();
            if !was_present {
                // Text was never there: the transition already holds. One
                // fresh frame documents it; no wait is needed.
                let frame = session.observe(quiet_ms)?;
                return Ok(CaptureOutcome {
                    reason: crate::backend::CaptureReason::TextAbsent,
                    met: true,
                    screen_seq: session.event_state().screen_seq,
                    output_seq: session.event_state().output_seq,
                    frame,
                    elapsed_ms: 0,
                    frames: None,
                });
            }
            loop {
                let frame = session.observe(quiet_ms.min(30))?;
                if !crate::capture::screen_contains(&frame, &text) {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::TextAbsent,
                        met: true,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                if start.elapsed() >= budget {
                    return Ok(CaptureOutcome {
                        reason: crate::backend::CaptureReason::Deadline,
                        met: false,
                        screen_seq: session.event_state().screen_seq,
                        output_seq: session.event_state().output_seq,
                        frame,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        frames: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Plan::Immediate => {
            let frame = session.observe(quiet_ms)?;
            Ok(CaptureOutcome {
                reason: crate::backend::CaptureReason::Deadline,
                met: true,
                screen_seq: session.event_state().screen_seq,
                output_seq: session.event_state().output_seq,
                frame,
                elapsed_ms: 0,
                frames: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // Lightweight session over a real python child; PTY machinery is
        // exercised in the conformance suites, here we need the sequencing.
        let id = "exec-test".to_string();
        let mut s = Session::new(id, "python3".to_string());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('go'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        s
    }

    #[test]
    fn execute_act_reports_settled_transition() {
        let mut s = session();
        let tx = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
            },
            60,
            1000,
            false,
        )
        .expect("execute act");
        assert_eq!(tx.settle, SettleStatus::Met, "{}", tx.settle_reason());
        assert!(tx.settled(), "legacy bool must agree with the status");
        assert_eq!(tx.name(), "key");
        // echoed char must appear in the after frame
        assert!(
            tx.after().viewport_text.iter().any(|r| r.contains('x')),
            "typed char must be echoed"
        );
    }

    #[test]
    fn execute_act_no_wait_skips_settle() {
        let mut s = session();
        let tx = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('y')),
            },
            60,
            1000,
            true,
        )
        .expect("execute act");
        // no_wait must be Skipped, not settled (re-review P1 fix 8).
        assert_eq!(tx.settle, SettleStatus::Skipped);
        assert!(
            !tx.settled(),
            "Skipped must not read as settled through the legacy bool"
        );
        assert_eq!(tx.settle_reason().as_str(), "no_wait");
        assert_eq!(tx.elapsed_ms, 0);
    }

    /// Anchor indices are real per-session monotonic counters (P1 fix 9),
    /// not hardcoded zeros.
    #[test]
    fn anchor_indices_are_monotonic() {
        let mut s = session();
        let a = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('a')),
            },
            60,
            1000,
            true,
        )
        .expect("act 1");
        let b = execute_act(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('b')),
            },
            60,
            1000,
            true,
        )
        .expect("act 2");
        let ia = a.anchor.index;
        let ib = b.anchor.index;
        assert!(ib > ia, "anchor index must increase: {} then {}", ia, ib);
    }

    /// A sensitive action's bytes must never reach the session's cast
    /// recording (leak fix, live half), while the PTY still receives them —
    /// the echo in the after-frame proves delivery.
    #[test]
    fn sensitive_send_bypasses_recording_but_reaches_pty() {
        let mut s = session();
        s.enable_recording(true);
        let secret = "hunter2-secret-payload";
        let tx = execute_act_with_visibility(
            &mut s,
            &CanonicalAction::Type {
                text: secret.to_string(),
            },
            80,
            1500,
            false,
            InputVisibility::Sensitive,
        )
        .expect("sensitive act");
        // PTY got the bytes: python echoes them back to the screen.
        assert!(
            tx.after()
                .viewport_text
                .iter()
                .any(|r: &String| r.contains(secret)),
            "payload must reach the PTY and be echoed"
        );
        // ...but not the recording.
        let cast = s
            .recorder()
            .expect("recorder")
            .lock()
            .expect("recorder lock")
            .to_ndjson()
            .join("\n");
        assert!(
            !cast.contains(secret),
            "sensitive payload must not leak into the recording"
        );
        // Unprotected sends still record (control).
        let _ = execute_act(
            &mut s,
            &CanonicalAction::Type {
                text: "visible-text".to_string(),
            },
            80,
            1500,
            false,
        )
        .expect("normal act");
        let cast2 = s
            .recorder()
            .expect("recorder")
            .lock()
            .expect("recorder lock")
            .to_ndjson()
            .join("\n");
        assert!(
            cast2.contains("visible-text"),
            "non-sensitive input must still be recorded"
        );
    }

    /// The mutual-exclusion between: a send failure in a sensitive act must
    /// tear down the whole transaction — the guard restores recording even
    /// though no `commit` was reached (Wave G review P1 18, error path).
    #[test]
    fn failed_sensitive_act_restores_recording_via_guard() {
        let mut s = session();
        s.enable_recording(true);
        // Stop the child so the next `send` fails; sending to a dead PTY is
        // the deterministic analogue of any mid-transaction failure.
        s.stop().expect("stop child");

        let err = execute_act_with_visibility(
            &mut s,
            &CanonicalAction::Type {
                text: "should-not-matter".to_string(),
            },
            40,
            1000,
            false,
            InputVisibility::Sensitive,
        );
        assert!(err.is_err(), "a send to a stopped child must fail");

        // Recording must be restored on the error path — a leaked suppression
        // would leave the session permanently blind.
        assert!(
            s.is_recording(),
            "the transaction guard must restore recording on a failed send"
        );
    }

    // ── CompletionPolicy wiring (review P0 rigidity) ──────────────────

    /// A `ProcessExit` completion declares "the action's real success signal is
    /// the child exiting", NOT a screen settle. `execute_act` must wait for the
    /// exit and report `Met` — not a spurious stable-screen timeout.
    #[test]
    fn process_exit_completion_waits_for_exit_not_screen_settle() {
        let mut s = Session::new("exec-exit".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import sys,time; print('bye'); sys.stdout.flush(); time.sleep(0.2)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        // Wait for initial output so the process is visibly alive before the act.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let tx = execute_act_with_completion(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('q')),
            },
            40,
            2000,
            false,
            InputVisibility::Normal,
            CompletionPolicy::ProcessExit,
        )
        .expect("act with process-exit completion");
        // The child exits ~200ms after start; the completion proves the exit.
        assert_eq!(
            tx.settle,
            SettleStatus::Met,
            "process-exit completion must report Met, got {}",
            tx.settle_reason()
        );
        assert!(
            !tx.after().process.running,
            "after a process-exit completion the child must be gone"
        );
        s.stop().ok();
    }

    /// A `MayBeSilent` completion MUST never produce a false `settled=false`:
    /// the action may legitimately change nothing observable (clipboard copy,
    /// an invisible toggle). The executor captures whatever the screen shows
    /// and reports `Met` — the exact rigidity the review called out.
    #[test]
    fn may_be_silent_never_reports_false_timeout() {
        let mut s = Session::new("exec-silent".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import time; print('quiet'); time.sleep(3)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(300));

        let tx = execute_act_with_completion(
            &mut s,
            &CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
            },
            40,
            300,
            false,
            InputVisibility::Normal,
            CompletionPolicy::MayBeSilent,
        )
        .expect("act");
        // Even if the typed char echoes, no *settle* was required; and if the
        // screen stayed quiet the action is still a success. Either way Met.
        assert_eq!(
            tx.settle,
            SettleStatus::Met,
            "MayBeSilent must report Met, never a false timeout"
        );
        s.stop().ok();
    }

    // ── CanonicalAction (Wave-2 item 10) ──────────────────────────────

    /// JSON round-trip preserves every payload byte: replay must execute
    /// the same action the caller sent, including modifiers and coords.
    #[test]
    fn canonical_action_json_roundtrip() {
        use crate::backend::{KeyCode, KeyModifiers, MouseButton, ScrollDirection};
        let actions = vec![
            CanonicalAction::Key {
                key: crate::backend::KeyEvent::with_modifiers(
                    KeyCode::Char('c'),
                    KeyModifiers::CTRL,
                ),
            },
            CanonicalAction::Keys {
                keys: vec![
                    crate::backend::KeyEvent::new(KeyCode::Tab),
                    crate::backend::KeyEvent::new(KeyCode::Enter),
                ],
            },
            CanonicalAction::Type {
                text: "hello 世界".into(),
            },
            CanonicalAction::Paste {
                text: "block\nof text".into(),
            },
            CanonicalAction::Raw {
                bytes: vec![0x1b, 0x5b, 0x41],
            },
            CanonicalAction::MouseClick {
                button: MouseButton::Right,
                x: 12,
                y: 34,
            },
            CanonicalAction::MouseDrag {
                button: MouseButton::Left,
                x: 1,
                y: 2,
            },
            CanonicalAction::MouseScroll {
                direction: ScrollDirection::Up,
                x: 5,
                y: 6,
            },
            CanonicalAction::Resize {
                cols: 120,
                rows: 40,
            },
            CanonicalAction::Signal { signal: 9 },
        ];
        for a in actions {
            let json = serde_json::to_string(&a).expect("serialize");
            let back: CanonicalAction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, a, "round-trip must be lossless for {:?}", a.name());
        }
    }

    /// Names match the action vocabulary already used across the ledger and
    /// audits; `to_input` translates losslessly.
    #[test]
    fn canonical_action_names_and_inputs() {
        use crate::backend::{Input, KeyCode, MouseButton};
        let click = CanonicalAction::MouseClick {
            button: MouseButton::Middle,
            x: 7,
            y: 8,
        };
        assert_eq!(click.name(), "mouse_click");
        match click.to_input() {
            Input::MouseClick { button, x, y } => {
                assert_eq!(button, MouseButton::Middle);
                assert_eq!((x, y), (7, 8));
            }
            _ => panic!("expected MouseClick input"),
        }

        let key = CanonicalAction::Key {
            key: crate::backend::KeyEvent::new(KeyCode::Tab),
        };
        assert_eq!(key.name(), "key");
        assert!(matches!(key.to_input(), Input::Key(_)));
    }

    /// `from_request` captures the full payload and deliberately drops
    /// transport fields; validation errors mirror the old builder.
    #[test]
    fn canonical_action_from_request() {
        use crate::mcp::params::TuiActRequest;
        let req = TuiActRequest::MouseClick {
            x: 3,
            y: 4,
            button: None,
            no_wait: Some(true),
            completion: None,
            wait_ms: Some(500),
            id: Some("s1".into()),
            guard: None,
        };
        let a = CanonicalAction::from_request(&req).expect("click");
        assert_eq!(a.name(), "mouse_click");
        match a {
            CanonicalAction::MouseClick { button, x, y } => {
                assert_eq!(button, crate::backend::MouseButton::Left, "default button");
                assert_eq!((x, y), (3, 4));
            }
            _ => panic!("wrong variant"),
        }

        let bad = TuiActRequest::Keys {
            keys: vec![],
            no_wait: None,
            completion: None,
            wait_ms: None,
            id: None,
            guard: None,
        };
        assert!(
            CanonicalAction::from_request(&bad).is_err(),
            "empty keys rejected"
        );

        let raw = TuiActRequest::Raw {
            raw: vec![],
            no_wait: None,
            completion: None,
            wait_ms: None,
            id: None,
            guard: None,
        };
        assert!(
            CanonicalAction::from_request(&raw).is_err(),
            "empty raw rejected"
        );
    }
}
