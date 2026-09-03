//! tui_observe / tui_wait / tui_assert: observation and causal waiting.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{build_wait, err, err_continued, ok};
use crate::mcp::params::*;
use crate::screen::diff;
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
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_observe(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiObserveParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::ObserveMode as OM;
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
    let selector = p.id.clone();
    let run = s.run.clone();
    s.with_sess(selector.as_deref(), move |sess| {
            // Lazy screen capture (re-review item 5): only the modes that
            // render a screen need to pay a settle cycle and advance the
            // session baseline. `changes`/`scrollback`/`search`/
            // `command_state` read their own retained state instead, so they
            // cost no PTY round-trip and do NOT move `previous` — a passive
            // read must not re-anchor someone's diff baseline.
            let idle = p.idle_ms.unwrap_or(80);
            // Returns the screen, or the error that already belongs to a
            // `return err(...)` in the calling arm (the callback body is not a
            // Result, so `?` is not available — the call sites match instead).
            let sweep = |s: &mut crate::session::Session| {
                match s.observe(idle) {
                    Ok(screen) => {
                        let mut run = run.lock().unwrap();
                        // Incremental event persistence (re-review Wave-2):
                        // events land in the run's on-disk log as they fire,
                        // via the run's own consumer cursor — not only at
                        // close. `bump_event` alone counted an estimate while
                        // real events sat in the queues; a crash between
                        // observe and close lost the whole log. `hold_events`
                        // is append-only and idempotent per event (seqs are
                        // unique), and the close path still drains whatever
                        // tail remained.
                        {
                            let cursor_key = format!("persistence:{}", s.id);
                            let from = run.event_cursor(&cursor_key).unwrap_or(0);
                            let batch = s.events_since(from);
                            if !batch.events.is_empty() {
                                let _ = run.hold_events(&s.id, batch.events);
                                run.set_event_cursor(&cursor_key, batch.cursor);
                            }
                        }
                        run.bump_event();
                        // Coverage ingestion rides every observation
                        // (re-review P1 item 17): native coverage events were
                        // absorbed into the session queue by observe itself,
                        // so the run ledger folds them here regardless of
                        // which observe mode triggered the sweep.
                        let batch = s.events_since(0);
                        let cursor_key = format!("coverage:{}", s.id);
                        let from = run.event_cursor(&cursor_key).unwrap_or(0);
                        let seq = batch.cursor;
                        if batch.cursor > from {
                            let evs = s.events_since(from);
                            // Wave 5 item 41: widget-targeted coverage events
                            // join the declaring node's app-attested source
                            // locus (same NSP channel), so coverage→source is
                            // exact. File targets stay plain.
                            for ev in &evs.events {
                                if let crate::events::TerminalEventKind::NativeEvent {
                                    event,
                                    target,
                                } = &ev.kind
                                {
                                    if event != "coverage" {
                                        continue;
                                    }
                                    let locus = s
                                        .native_source_for_widget(target)
                                        .filter(|_| !(target.starts_with("src/") || target.contains(".rs:") || target.contains('/')));
                                    match locus {
                                        Some(sr) => {
                                            let _ = run.record_coverage_event_with_identity(
                                                &s.id, target, sr,
                                            );
                                        }
                                        None => {
                                            let _ = run.record_coverage_event(&s.id, target);
                                        }
                                    }
                                }
                            }
                            run.set_event_cursor(&cursor_key, seq);
                        }
                        Ok(screen)
                    }
                    Err(e) => Err((ErrorCategory::BackendError, e.to_string())),
                }
            };
            // First error from a lazy screen capture short-circuits the arm.
            macro_rules! cap {
                () => {
                    match sweep(sess) {
                        Ok(screen) => screen,
                        Err((c, m)) => return err(c, m),
                    }
                };
            }

            if mode.known().is_none() {
                unreachable!("validated above");
            }
        match mode.known().copied().unwrap() {
            OM::Summary => {
                let screen = cap!();
                // Compact summary for agent consumption (spec 36). Avoids
                // returning the full viewport text (which could be 160x50).
                // Use mode=screen for full text.
                //
                // Fused semantic truth (re-review Wave-4): summary reports
                // the SAME analysis every other semantic-bearing mode sees —
                // one cached detection pass + native overlay — so `focus`
                // here is the app's declared focus when it cooperates, not
                // an inference-only verdict the nodes mode contradicts.
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                let dialog_count = sem
                    .regions
                    .iter()
                    .filter(|r| r.kind == semantic::RegionKind::Dialog)
                    .count();
                let focused = sem.focus.control.clone();
                // Re-review P1 (items 17+18): a read no longer mutates
                // behavioral evidence. Focus edges come from interaction
                // transactions (`record_interaction`), never from a summary
                // poll — "between two arbitrary reads" provenance ("unknown")
                // polluted the graph. Native coverage ingestion moved to the
                // frame pipeline (observe itself folds native events into the
                // session queue), so tool selection cannot shift coverage
                // accounting either.
                ok(json!({
                    "screen": format!("{}x{}", screen.cols, screen.rows),
                    "title": screen.title,
                    "process": {
                        "running": screen.process.running,
                        "exit_code": screen.process.exit_code,
                    },
                    "focus": focused,
                    "focus_confidence": sem.focus.confidence,
                    "regions": sem.regions.len(),
                    "controls": sem.controls.len(),
                    "dialogs": dialog_count,
                    // Wave F item 55: OSC8 hyperlinks surfaced in the summary —
                    // the URI is the actionable fact, the span is where to click.
                    "hyperlinks": screen.hyperlinks.iter().map(|l| json!({
                        "uri": l.uri,
                        "id": l.id,
                        "start": l.start,
                        "end": l.end,
                    })).collect::<Vec<_>>(),
                    "structure_hash": screen.structure_hash,
                    "visual_hash": screen.visual_hash,
                    "cursor": screen.cursor,
                    // Wave F items 58–63: native cooperation status — the
                    // app's own view of itself, when it provides one.
                    "native": {
                        "active": sess.native_channel().latest.is_some(),
                        "framework": sess.native_channel().framework,
                    },
                    // Cache layers (re-review item 41): the structural cache's
                    // health is evidence — a hit rate near 1.0 on a static
                    // screen says the fused path is not re-running detectors.
                    "semantic_cache": {
                        "hits": sess.semantic_cache_hits(),
                        "misses": sess.semantic_cache_misses(),
                        "hit_rate": sess.semantic_cache_hit_rate(),
                    },
                    // Reactive fused commit (re-review item 52): how many
                    // fused reads the memo served — the interaction pass +
                    // native overlay did not re-run for these.
                    "fused_memo_hits": sess.fused_memo_hits(),
                }))
            }
            OM::Screen => {
                let screen = cap!();
                ok(json!({ "viewport_text": screen.viewport_text }))
            }
            OM::Cells => {
                let screen = cap!();
                ok(json!({ "cells": screen.cells }))
            }
            OM::Semantic => {
                let screen = cap!();
                // Fused truth: the flat semantic surface carries the native
                // overlay too (focus rewrite, native enable/value/label on
                // matched controls) — the same facts the nodes tree sees.
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                ok(json!({ "semantic": sem }))
            }
            // The hierarchical rendering (re-review Wave-4): regions nested
            // per containment, controls inside their regions, focus and
            // screen-level components attached. This is the shape to compare
            // against an intended design without re-deriving containment.
            // Built from the FUSED flat analysis, so a native focus
            // declaration is reflected here, not just in nodes mode.
            OM::Tree => {
                let screen = cap!();
                let (sem, _tree, _report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                let tree = semantic::build_state_tree(
                    screen.cols,
                    screen.rows,
                    &sem.regions,
                    &sem.controls,
                    &sem.focus,
                    &sem.components,
                );
                ok(json!({ "tree": tree, "rendered": tree.render() }))
            }
            // Wave C (items 16-30): the general SemanticNode tree — one node
            // type for regions, controls, widget internals (table rows/cells,
            // tree items, scroll edges), hyperlinks, and help hints, with
            // modal layering and provenance-tracked enabled state.
            // Wave F (items 58–63): when the app cooperates via the native
            // side channel, its real tree is merged over inference and the
            // merge report names what matched.
            OM::Nodes => {
                let screen = cap!();
                // Fused truth: the tree comes from the same cached detection
                // pass (no second detector run) and the same native overlay
                // as every other semantic-bearing mode.
                let (sem, tree, native_report) = match sess.fused_frame() {
                    Some(t) => t,
                    None => (
                        semantic::analyze(&screen),
                        semantic::build_tree(&screen),
                        crate::semantic::native::NativeOverlayReport::default(),
                    ),
                };
                let _ = &sem;
                ok(json!({
                    "tree": tree,
                    "rendered": tree.render(),
                    "layers": tree.layers,
                    "native": {
                        "active": native_report.active(),
                        // Item 35: availability vs activity vs health.
                        "adapter_status": sess.adapter_status(),
                        "framework": sess.native_channel().framework,
                        "app": sess.native_channel().app,
                        "matched": native_report.matched,
                        "native_only": native_report.native_only,
                        "frames_accepted": sess.native_channel().frames_accepted,
                        "frames_invalid": sess.native_channel().frames_invalid,
                    },
                }))
            }
            OM::Diff => {
                let screen = cap!();
                // ONE diff path (re-review item 7): the sweep above stashed
                // the prior frame in `previous`, so this is a real
                // previous→current comparison through the canonical
                // `screen::diff`, returning the same `Transition` shape used
                // by InteractionTransaction (semantic diff included — no
                // second, MCP-local semantic-diff implementation).
                match sess.previous() {
                    Some(before) => {
                        let tr = diff(before, &screen);
                        ok(json!({
                            "since": "previous_observation",
                            "transition": tr,
                        }))
                    }
                    None => ok(json!({
                        "since": null,
                        "note": "no prior frame available; observe twice, then diff",
                        "structure_hash": screen.structure_hash,
                        "visual_hash": screen.visual_hash,
                    })),
                }
            }
            // Incremental read (Wave B item 13): return only what changed
            // since the named consumer's cursor, with dirty rows — far more
            // token-efficient than full-screen rereads for monitoring.
            OM::Changes => {
                let consumer = p.consumer.clone().unwrap_or_else(|| "hermes".to_string());
                let batch = sess.events_for_consumer(&consumer);
                ok(json!({
                    "consumer": consumer,
                    "cursor": batch.cursor,
                    "gap": batch.gap,
                    "first_available": batch.first_available,
                    "events": batch.events,
                    "note": if batch.events.is_empty() {
                        Some("no changes since cursor".to_string())
                    } else {
                        None
                    },
                }))
            }
            // Wave F item 53: real scrollback (viewport + history). A backend
            // that cannot retain history still answers — honestly, with the
            // capability state named.
            OM::Scrollback => {
                let lines = sess
                    .backend_scrollback()
                    .map_err(|e| err(ErrorCategory::BackendError, e.to_string()))
                    .unwrap_or_default();
                let supported = sess.capabilities().scrollback;
                ok(json!({
                    "supported": supported,
                    "lines": lines,
                    "count": lines.len(),
                    "note": if supported { None } else {
                        Some("this backend retains no history; the array is empty because there is genuinely nothing to return".to_string())
                    },
                }))
            }
            // Wave F item 53: search viewport + scrollback.
            OM::Search => {
                let Some(query) = p.text.clone().or(p.query.clone()) else {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "mode=search requires 'text' (the query)",
                    );
                };
                match sess.backend_search(&query) {
                    Ok(hits) => ok(json!({
                        "query": query,
                        "hits": hits,
                        "count": hits.len(),
                    })),
                    Err(e) => err(ErrorCategory::BackendError, e.to_string()),
                }
            }
            // Wave F item 54: OSC 133 shell-integration state.
            OM::CommandState => {
                match sess.backend_command_state() {
                    Some(cs) => ok(json!({ "command_state": cs })),
                    None => ok(json!({
                        "command_state": null,
                        "note": "no shell integration observed (no OSC 133 traffic); command waits cannot resolve"
                    })),
                }
            }
            // Re-review item 15: the converged event history — what happened,
            // in order, filtered. This is the "it broke somewhere in the
            // last 30 seconds; what happened?" tool: window by seq, filter
            // by kind, and every event carries its timestamp, session
            // generation, and kind so an agent can correlate with frames
            // and transactions. The session event queue is the ONE store
            // (item 16); this projection names its events in the converged
            // bus vocabulary.
            OM::History => {
                let query = crate::events::HistoryQuery {
                    since_seq: p.since_seq.unwrap_or(0),
                    until_seq: p.until_seq,
                    limit: p.limit.map(|l| (l as usize).min(4096)),
                    event_types: p.event_types.clone().unwrap_or_default(),
                };
                let all = sess.all_events();
                let batch = crate::events::project_history(&all, &query, sess.events_evicted());
                ok(json!({
                    "history": batch.events.iter().map(|ev| json!({
                        "seq": ev.seq,
                        "at": ev.at,
                        "source": ev.source.name(),
                        "kind": ev.kind.name(),
                        "detail": ev.kind,
                    })).collect::<Vec<_>>(),
                    "cursor": batch.cursor,
                    "gap": batch.gap,
                    "returned": batch.events.len(),
                    "note": if batch.gap {
                        "the ring evicted earlier events; this window is partial"
                    } else { "" },
                }))
            }
            // Wave-2 (protocol diagnostics): decode the backend's retained
            // RAW output window into a protocol trace — "what did this TUI
            // actually emit?", with OSC/DCS payloads redacted by default.
            OM::Protocol => {
                let (bytes, cap, dropped) = sess.raw_output_window();
                if cap == 0 {
                    return err(
                        ErrorCategory::Unsupported,
                        "this backend does not retain raw output; the protocol trace needs the portable-pty or line-cli engine",
                    );
                }
                let trace = crate::protocol::ProtocolTrace::decode(&bytes);
                let ops = trace
                    .ops
                    .iter()
                    .map(|op| serde_json::to_value(op).unwrap_or_default())
                    .collect::<Vec<_>>();
                ok(json!({
                    "window": {
                        "bytes": bytes.len(),
                        "ring_capacity": cap,
                        "dropped_head_bytes": dropped,
                        "complete": dropped == 0,
                    },
                    "op_count": ops.len(),
                    "ops": ops,
                    "modes": trace.modes,
                }))
            }
            // Wave-2 (streams): genuine stdout/stderr separation — the pipe
            // engine's per-stream line stores. Other engines answer honestly
            // that they interleave (a PTY merges the streams by construction).
            OM::Streams => {
                if let crate::session::state::BackendKind::Pipe = sess.backend_kind {
                    // The pipe backend exposes its stores through the
                    // session's typed engine accessors.
                    let (out, errl) = sess.pipe_streams();
                    ok(json!({
                        "engine": "pipe",
                        "stdout_lines": out,
                        "stderr_lines": errl,
                    }))
                } else {
                    ok(json!({
                        "engine": sess.backend_kind,
                        "note": "a PTY interleaves stdout and stderr by construction; per-stream separation requires the pipe engine",
                    }))
                }
            }
            // Wave-2 (terminal modes): the negotiated-mode timeline the
            // protocol decoder reconstructed from the raw window (DECSET/
            // DECRST: alt screen, cursor visibility, mouse, SGR, bracketed
            // paste, synchronized update, application cursor keys).
            OM::TerminalModes => {
                let (bytes, cap, dropped) = sess.raw_output_window();
                if cap == 0 {
                    return err(
                        ErrorCategory::Unsupported,
                        "this backend does not retain raw output; the mode timeline needs the portable-pty or line-cli engine",
                    );
                }
                let trace = crate::protocol::ProtocolTrace::decode(&bytes);
                // Tri-state fold (re-review item 20): an incomplete window
                // (dropped head) seeds Unknown — "no DECSET in the retained
                // bytes" is not evidence a mode is off. A complete history
                // seeds Disabled honestly.
                let complete = dropped == 0;
                let states =
                    crate::protocol::fold_mode_states(&trace.modes, complete);
                let states_json: serde_json::Map<String, serde_json::Value> = states
                    .iter()
                    .map(|(m, st)| {
                        (
                            m.to_string(),
                            json!({
                                "state": st,
                                "unverified": *st == crate::protocol::KnownModeState::Unknown,
                            }),
                        )
                    })
                    .collect();
                ok(json!({
                    "window": {
                        "bytes": bytes.len(),
                        "ring_capacity": cap,
                        "dropped_head_bytes": dropped,
                        "complete": complete,
                    },
                    "modes": trace.modes,
                    "states": states_json,
                    "note": if complete { "" } else {
                        "the raw ring dropped its head: modes never seen in the window are UNKNOWN, not disabled — audits treat them as unverified"
                    },
                }))
            }
        }
        })
        .await
        .unwrap_or_else(|e| e)
}
