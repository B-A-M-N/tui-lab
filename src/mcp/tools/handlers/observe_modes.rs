//! `tui_observe` mode dispatch (review §15 follow-up: the largest handler
//! family got its own file). One arm per [`ObserveMode`]; every arm runs
//! inside the session's actor with `sess` live. The lazy screen capture
//! (`sweep`) stays in the caller — only screen-rendering modes pay a settle
//! cycle — and arms opt in through the [`cap`] helper the caller passes.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::ObserveMode as OM;
use crate::mcp::params::TuiObserveParams;
use rmcp::model::CallToolResult;
use rmcp::serde_json::json;

/// Body of one observe mode. Split from `super::tui_observe`'s match so the
/// mode vocabulary has a file of its own; `screen` is `None` for modes that
/// skip the sweep (retained-state readers) — a screen-rendering arm never
/// sees `None` because the caller only sweeps for those exact modes.
pub(crate) fn observe_mode_arm(
    sess: &mut crate::session::Session,
    mode: OM,
    p: &TuiObserveParams,
    screen: Option<crate::screen::ScreenState>,
) -> CallToolResult {
    use crate::semantic;
    // Fused truth helper (re-review Wave-4): every semantic-bearing mode
    // reports the SAME analysis — one cached detection pass + native
    // overlay — never an inference-only view.
    let fused = |sess: &crate::session::Session, screen: &crate::screen::ScreenState| match sess
        .fused_frame()
    {
        Some(t) => t,
        None => (
            semantic::analyze(screen),
            semantic::build_tree(screen),
            semantic::native::NativeOverlayReport::default(),
        ),
    };
    let screen = || {
        screen
            .clone()
            .expect("screen-rendering arm without a sweep")
    };
    match mode {
        OM::Summary => {
            let screen = screen();
            let (sem, _tree, _report) = fused(sess, &screen);
            let dialog_count = sem
                .regions
                .iter()
                .filter(|r| r.kind == semantic::RegionKind::Dialog)
                .count();
            let focused = sem.focus.control.clone();
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
                "hyperlinks": screen.hyperlinks.iter().map(|l| json!({
                    "uri": l.uri,
                    "id": l.id,
                    "start": l.start,
                    "end": l.end,
                })).collect::<Vec<_>>(),
                "structure_hash": screen.structure_hash,
                "visual_hash": screen.visual_hash,
                "cursor": screen.cursor,
                "native": {
                    "active": sess.native_channel().latest.is_some(),
                    "framework": sess.native_channel().framework,
                },
                "semantic_cache": {
                    "hits": sess.semantic_cache_hits(),
                    "misses": sess.semantic_cache_misses(),
                    "hit_rate": sess.semantic_cache_hit_rate(),
                },
                "fused_memo_hits": sess.fused_memo_hits(),
            }))
        }
        OM::Screen => ok(json!({ "viewport_text": screen().viewport_text })),
        OM::Cells => ok(json!({ "cells": screen().cells })),
        OM::Semantic => {
            let screen = screen();
            let (sem, _tree, _report) = fused(sess, &screen);
            ok(json!({ "semantic": sem }))
        }
        OM::Tree => {
            let screen = screen();
            let (sem, _tree, _report) = fused(sess, &screen);
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
        OM::Nodes => {
            let screen = screen();
            let (_sem, tree, native_report) = fused(sess, &screen);
            ok(json!({
                "tree": tree,
                "rendered": tree.render(),
                "layers": tree.layers,
                "native": {
                    "active": native_report.active(),
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
            let screen = screen();
            match sess.previous() {
                Some(before) => {
                    let tr = crate::screen::diff(before, &screen);
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
        OM::Scrollback => {
            // Audit P1-43: a backend ERROR used to be swallowed by
            // `unwrap_or_default()`, so a failed read reported
            // `supported: true` with an empty array — indistinguishable from
            // a genuinely empty history. An error is surfaced as an error;
            // only a SUCCESSFUL read of nothing is an empty array.
            let lines = match sess.backend_scrollback() {
                Ok(lines) => lines,
                Err(e) => {
                    return err(
                        ErrorCategory::BackendError,
                        format!("scrollback read failed: {e}"),
                    )
                }
            };
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
        OM::CommandState => match sess.backend_command_state() {
            Some(cs) => ok(json!({ "command_state": cs })),
            None => ok(json!({
                "command_state": null,
                "note": "no shell integration observed (no OSC 133 traffic); command waits cannot resolve"
            })),
        },
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
        OM::Protocol => {
            // Audit P1-44: a failed raw-window read is an error, not an
            // empty window dressed up as "no capture".
            let (bytes, cap, dropped) = match sess.raw_output_window() {
                Ok(w) => w,
                Err(e) => {
                    return err(
                        ErrorCategory::BackendError,
                        format!("raw output window read failed: {e}"),
                    )
                }
            };
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
        OM::Streams => {
            if let crate::session::state::BackendKind::Pipe = sess.backend_kind {
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
        OM::TerminalModes => {
            let (bytes, cap, dropped) = match sess.raw_output_window() {
                Ok(w) => w,
                Err(e) => {
                    return err(
                        ErrorCategory::BackendError,
                        format!("raw output window read failed: {e}"),
                    )
                }
            };
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
            let states = crate::protocol::fold_mode_states(&trace.modes, complete);
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
}
