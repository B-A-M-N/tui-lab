//! tui_observe / tui_wait / tui_assert: observation and causal waiting.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{build_wait, err, err_continued, ok};
use crate::mcp::params::*;
use crate::semantic;
use rmcp::serde_json::json;

/// Body of `tui_assert` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_assert(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiAssertParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    // Wave E: `assertion: "oracle"` evaluates a declarative oracle
    // expression (shared language with contracts and audits). Any other
    // assertion name goes through the canonical assert executor.
    use crate::mcp::params::AssertAssertion as AA;
    let is_oracle = matches!(p.assertion.known(), Some(AA::Oracle));
    if is_oracle && p.text.is_none() && p.reference.is_none() {
        return err(
            ErrorCategory::InvalidRequest,
            "oracle assertion requires the expression in 'text' (e.g. text: \"modal_open()\")",
        );
    }
    let selector = p.id.clone();
    let run = s.run.clone();
    s.with_sess(selector.as_deref(), move |sess| {
        let screen = match sess.observe(40) {
            Ok(s) => s,
            Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
        };
        if is_oracle {
            let expr = p
                .text
                .clone()
                .or_else(|| p.reference.clone())
                .expect("validated above");
            let sem = sess
                .fused_frame()
                .map(|(s, _, _)| s)
                .unwrap_or_else(|| semantic::analyze(&screen));
            let outcome = crate::design::eval_static(&expr, &screen, &sem);
            {
                // Scoped to the resolved session generation (item 5).
                let (sid, gen) = (sess.id.clone(), sess.generation);
                let mut run = run.lock().unwrap();
                let _ = run.record_scenario_assert(
                    &sid,
                    gen,
                    serde_json::json!({ "assertion": "oracle", "text": expr }),
                );
            }
            if outcome.passed {
                ok(json!({
                    "passed": true,
                    "assertion": "oracle",
                    "expression": expr,
                    "detail": outcome.detail,
                    "flavor": outcome.flavor,
                }))
            } else if outcome.parse_error.is_some() {
                // A malformed expression is a caller error, not a UI failure.
                err(ErrorCategory::InvalidRequest, outcome.detail)
            } else {
                err_continued(
                    ErrorCategory::AssertionFailed,
                    format!("oracle '{}' failed: {}", expr, outcome.detail),
                )
            }
        } else {
            // Go through the canonical assert executor (re-review P0).
            let (passed, detail, invalid) = crate::execution::execute_assert(&p, &screen);
            {
                // Scoped to the resolved session generation (item 5).
                let (sid, gen) = (sess.id.clone(), sess.generation);
                let mut run = run.lock().unwrap();
                let _ = run.record_scenario_assert(
                    &sid,
                    gen,
                    serde_json::to_value(&p).unwrap_or_default(),
                );
            }
            if passed {
                ok(json!({ "passed": true, "assertion": p.assertion_name() }))
            } else if let Some(ErrorCategory::InvalidRequest) = invalid {
                // Unknown assertion name (or missing required param): a caller error,
                // NOT a UI failure (spec section 37).
                err(ErrorCategory::InvalidRequest, detail)
            } else {
                err_continued(ErrorCategory::AssertionFailed, detail)
            }
        }
    })
    .await
    .unwrap_or_else(|e| e)
}

/// Body of `tui_wait` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_wait(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiWaitParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    // Re-review item 17: condition=event waits on the declarative event
    // predicate over the converged history — a session-queue concern,
    // not a backend read condition.
    if matches!(
        p.condition.known(),
        Some(crate::mcp::params::WaitCondition::Event)
    ) {
        let predicate = match p.event.clone() {
            Some(pred) => pred,
            None => return err(
                ErrorCategory::InvalidRequest,
                "condition=event requires the 'event' predicate object (kinds/contains/since_seq)",
            ),
        };
        if predicate.kinds.is_empty() && predicate.contains.is_none() {
            return err(
                    ErrorCategory::InvalidRequest,
                    "condition=event predicate is empty: supply 'kinds' and/or 'contains' (since_seq optional)",
                );
        }
        let selector = p.id.clone();
        let budget = p.budget_ms.unwrap_or(5000);
        return s
            .with_sess(
                selector.as_deref(),
                move |sess| match crate::execution::execute_wait_event(sess, &predicate, budget) {
                    Ok(out) => ok(json!({
                        "met": out.met,
                        "timeout": !out.met,
                        "matched_seq": out.matched_seq,
                        "matched_at": out.matched_at,
                        "last_seq": out.last_seq,
                        "elapsed_ms": out.elapsed_ms,
                    })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                },
            )
            .await
            .unwrap_or_else(|e| e);
    }
    let cond = match build_wait(&p) {
        Some(c) => c,
        None => return err(ErrorCategory::InvalidRequest, "unsupported wait condition"),
    };
    let selector = p.id.clone();
    let run = s.run.clone();
    // Go through the canonical wait executor (re-review P0), inside the
    // session actor.
    s.with_sess(selector.as_deref(), move |sess| {
        match crate::execution::execute_wait(sess, cond, p.budget_ms.unwrap_or(5000)) {
            Ok(out) => {
                // Scoped to the resolved session generation (item 5).
                {
                    let (sid, gen) = (sess.id.clone(), sess.generation);
                    let mut run = run.lock().unwrap();
                    let _ = run.record_event(&sid, "wait");
                    let _ = run.record_scenario_wait(
                        &sid,
                        gen,
                        serde_json::to_value(&p).unwrap_or_default(),
                    );
                }
                ok(json!({
                    "met": out.met,
                "timeout": !out.met,
                "reason": format!("{:?}", out.reason),
                "elapsed_ms": out.elapsed_ms,
                "screen_seq": out.screen_seq,
                "output_seq": out.output_seq,
                    "state": {
                        "structure_hash": out.state.structure_hash,
                        "visual_hash": out.state.visual_hash,
                        "process": {
                            "running": out.state.process.running,
                            "exit_code": out.state.process.exit_code,
                        },
                    },
                }))
            }
            Err(e) => err(ErrorCategory::BackendError, e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| e)
}

/// Body of `tui_observe` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. This file keeps the session
/// lifecycle (actor entry, the lazy sweep with its persistence/coverage
/// side effects) while the per-mode rendering lives in [`observe_modes`]
/// (review §15 follow-up: the biggest handler family got a file of its
/// own).
pub(crate) async fn tui_observe(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiObserveParams>,
) -> rmcp::model::CallToolResult {
    use crate::mcp::params::ObserveMode as OM;
    let p = p.0;
    let mode = p
        .mode
        .clone()
        .unwrap_or(crate::mcp::params::Known::Known(OM::Summary));
    if mode.known().is_none() {
        let bad = match &mode {
            crate::mcp::params::Known::Other(o) => o.clone(),
            _ => String::new(),
        };
        return err(
            ErrorCategory::InvalidRequest,
            format!(
                "unknown observe mode '{}' (expected one of: {})",
                bad,
                <OM as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
            ),
        );
    }
    let mode = mode.known().copied().unwrap();
    let selector = p.id.clone();
    let run = s.run.clone();
    s.with_sess(selector.as_deref(), move |sess| {
        // Lazy screen capture (re-review item 5): only the modes that
        // render a screen need to pay a settle cycle and advance the
        // session baseline. `changes`/`scrollback`/`search`/
        // `command_state` read their own retained state instead, so they
        // cost no PTY round-trip and do NOT move `previous` — a passive
        // read must not re-anchor someone's diff baseline.
        let screen = if mode_needs_sweep(mode) {
            match sweep(sess, &run, p.idle_ms.unwrap_or(80)) {
                Ok(screen) => Some(screen),
                Err((c, m)) => return err(c, m),
            }
        } else {
            None
        };
        super::observe_modes::observe_mode_arm(sess, mode, &p, screen, &run)
    })
    .await
    .unwrap_or_else(|e| e)
}

/// Whether a mode renders the screen (and so pays the sweep): every
/// semantic/frame-bearing mode. The retained-state readers do not — they
/// cost no PTY round-trip and leave the caller's diff baseline alone.
fn mode_needs_sweep(mode: crate::mcp::params::ObserveMode) -> bool {
    use crate::mcp::params::ObserveMode as OM;
    matches!(
        mode,
        OM::Summary
            | OM::Screen
            | OM::Cells
            | OM::Semantic
            | OM::Tree
            | OM::Nodes
            | OM::Diff
            | OM::Inspect
    )
}

/// The lazy sweep: one settle cycle, plus its mandatory side effects —
/// incremental event persistence (re-review Wave-2) and native coverage
/// ingestion (re-review P1 item 17) ride EVERY observation, regardless of
/// which mode triggered it.
fn sweep(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>,
    idle: u64,
) -> Result<crate::screen::ScreenState, (ErrorCategory, String)> {
    let screen = match sess.observe(idle) {
        Ok(screen) => screen,
        Err(e) => return Err((ErrorCategory::BackendError, e.to_string())),
    };
    let mut run = run.lock().unwrap();
    // Incremental event persistence (re-review Wave-2): events land in the
    // run's on-disk log as they fire, via the run's own consumer cursor —
    // not only at close. `hold_events` is append-only and idempotent per
    // event (seqs are unique), and the close path still drains whatever
    // tail remained.
    {
        let cursor_key = format!("persistence:{}", sess.id);
        let from = run.event_cursor(&cursor_key).unwrap_or(0);
        let batch = sess.events_since(from);
        if !batch.events.is_empty() {
            let _ = run.hold_events(&sess.id, batch.events);
            run.set_event_cursor(&cursor_key, batch.cursor);
        }
    }
    run.bump_event();
    // Coverage ingestion rides every observation (re-review P1 item 17):
    // native coverage events were absorbed into the session queue by
    // observe itself, so the run ledger folds them here regardless of
    // which observe mode triggered the sweep.
    let batch = sess.events_since(0);
    let cursor_key = format!("coverage:{}", sess.id);
    let from = run.event_cursor(&cursor_key).unwrap_or(0);
    let seq = batch.cursor;
    if batch.cursor > from {
        let evs = sess.events_since(from);
        // Wave 5 item 41: widget-targeted coverage events join the
        // declaring node's app-attested source locus (same NSP channel),
        // so coverage→source is exact. File targets stay plain.
        for ev in &evs.events {
            if let crate::events::TerminalEventKind::NativeEvent { event, target } = &ev.kind {
                if event != "coverage" {
                    continue;
                }
                let locus = sess.native_source_for_widget(target).filter(|_| {
                    !(target.starts_with("src/") || target.contains(".rs:") || target.contains('/'))
                });
                match locus {
                    Some(sr) => {
                        let _ = run.record_coverage_event_with_identity(&sess.id, target, sr);
                    }
                    None => {
                        let _ = run.record_coverage_event(&sess.id, target);
                    }
                }
            }
        }
        run.set_event_cursor(&cursor_key, seq);
    }
    Ok(screen)
}
