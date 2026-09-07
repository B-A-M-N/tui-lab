//! `tui_observe` mode dispatch (review §15 follow-up: the largest handler
//! family got its own file). One arm per [`ObserveMode`]; every arm runs
//! inside the session's actor with `sess` live. The lazy screen capture
//! (`sweep`) stays in the caller — only screen-rendering modes pay a settle
//! cycle — and arms opt in through the [`cap`] helper the caller passes.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_with_details, ok};
use crate::mcp::params::ObserveMode as OM;
use crate::mcp::params::TuiObserveParams;
use rmcp::model::CallToolResult;
use rmcp::serde_json::json;

/// Body of one observe mode. Split from `super::tui_observe`'s match so the
/// mode vocabulary has a file of its own; `screen` is `None` for modes that
/// skip the sweep (retained-state readers) — a screen-rendering arm never
/// sees `None` because the caller only sweeps for those exact modes. `run`
/// is the run handle, for arms that cite run-held evidence (finding 36's
/// frame id + contract verdict).
pub(crate) fn observe_mode_arm(
    sess: &mut crate::session::Session,
    mode: OM,
    p: &TuiObserveParams,
    screen: Option<crate::screen::ScreenState>,
    run: &std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>,
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
        OM::Inspect => {
            // Finding 36: the first-class inspection view. One call
            // answers "what is this component, what can it do, where is
            // it in source, what state is it in, and what currently
            // violates the design?" — frame + semantic identity, per-
            // control facts (stable ids, bounds, state, affordances,
            // source loci), the fused native overlay health, and the
            // loaded contract's verdict on THIS frame.
            let screen = screen();
            let (sem, tree, native_report) = fused(sess, &screen);
            // ── target narrowing ──────────────────────────────────────
            // `target` picks ONE control by stable id, unique label
            // (exact first, then substring), or native id. Ambiguity is
            // reported with the candidates — never a first-pick.
            let target = p.target.as_deref().map(str::trim).filter(|t| !t.is_empty());
            let selected: Vec<&crate::semantic::Control> = match target {
                None => sem.controls.iter().collect(),
                Some(t) => {
                    let by_id: Vec<&crate::semantic::Control> =
                        sem.controls.iter().filter(|c| c.id == t).collect();
                    if by_id.len() == 1 {
                        by_id
                    } else {
                        let tl = t.to_lowercase();
                        let by_native: Vec<&crate::semantic::Control> = sem
                            .controls
                            .iter()
                            .filter(|c| {
                                c.source == "native" && {
                                    // Native ids ride the app's own node ids; the
                                    // overlay stores them on the tree nodes.
                                    tree.root
                                        .find(&c.id)
                                        .and_then(|n| n.identity.as_ref())
                                        .and_then(|i| i.native_id.as_deref())
                                        .map(|nid| {
                                            nid == t
                                                || nid.strip_prefix('#').unwrap_or(nid)
                                                    == t.strip_prefix('#').unwrap_or(t)
                                        })
                                        .unwrap_or(false)
                                }
                            })
                            .collect();
                        let exact: Vec<&crate::semantic::Control> = sem
                            .controls
                            .iter()
                            .filter(|c| c.label.to_lowercase() == tl)
                            .collect();
                        let chosen = if !by_native.is_empty() {
                            by_native
                        } else {
                            exact
                        };
                        if chosen.len() == 1 {
                            chosen
                        } else {
                            let sub: Vec<&crate::semantic::Control> = sem
                                .controls
                                .iter()
                                .filter(|c| c.label.to_lowercase().contains(&tl))
                                .collect();
                            match sub.len() {
                                1 => sub,
                                _ => {
                                    // 0 or ambiguous: name the candidates.
                                    let candidates: Vec<serde_json::Value> = if sub.len() > 1 {
                                        sub.iter()
                                            .map(|c| json!({ "id": c.id, "label": c.label }))
                                            .collect()
                                    } else {
                                        crate::intent::nearest_candidates(
                                            &sem.controls,
                                            &crate::intent::ActionTarget::Text {
                                                text: t.to_string(),
                                            },
                                        )
                                        .into_iter()
                                        .take(5)
                                        .map(|cs| json!({ "id": cs.id, "label": cs.label }))
                                        .collect()
                                    };
                                    return err_with_details(
                                        ErrorCategory::InvalidRequest,
                                        if sub.len() > 1 {
                                            format!(
                                                "target '{t}' is ambiguous: {} controls match",
                                                sub.len()
                                            )
                                        } else {
                                            format!("target '{t}' not found")
                                        },
                                        json!({ "candidates": candidates }),
                                    );
                                }
                            }
                        }
                    }
                }
            };
            // ── per-control facts ─────────────────────────────────────
            let controls_json: Vec<serde_json::Value> = selected
                .iter()
                .map(|c| {
                    // The joined identity for this control, when the fused
                    // tree carries one (native/contract/source joins).
                    let identity = tree
                        .root
                        .find(&c.id)
                        .and_then(|n| n.identity.clone());
                    let source: Vec<serde_json::Value> = identity
                        .as_ref()
                        .map(|i| {
                            i.source_refs
                                .iter()
                                .map(|sr| {
                                    json!({
                                        "location": sr.location(),
                                        "symbol": sr.symbol,
                                        "confidence": sr.confidence,
                                        "source": sr.source,
                                        "provenance": sr.provenance.name(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // The invocations this control actually supports —
                    // declared native verbs first, conventional inference
                    // otherwise (finding 8 semantics).
                    let affordances: Vec<serde_json::Value> = tree
                        .root
                        .find(&c.id)
                        .map(|n| {
                            n.affordances
                                .iter()
                                .map(|a| serde_json::to_value(a).unwrap_or_default())
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "id": c.id,
                        "kind": c.kind,
                        "label": c.label,
                        "value": c.value,
                        "bounds": { "x": c.bounds.x, "y": c.bounds.y, "w": c.bounds.width, "h": c.bounds.height },
                        "region_id": c.region_id,
                        "state": {
                            "focusable": c.focusable,
                            "focused": c.focused,
                            "enabled": c.enabled,
                            "selected": c.selected,
                            "checked": c.checked,
                        },
                        "shortcut": c.shortcut,
                        "confidence": c.confidence,
                        "source": c.source,
                        "identity": identity,
                        "source_refs": source,
                        "affordances": affordances,
                    })
                })
                .collect();
            // ── frame + semantic identity ─────────────────────────────
            let frame_record = run_frame_of(sess, run);
            let clipped: Vec<&crate::semantic::Region> = sem
                .regions
                .iter()
                .filter(|r| r.clipping_state != crate::semantic::ClippingState::None)
                .collect();
            // ── contract verdict on this frame ────────────────────────
            // Component presence checks are static (no driving), so the
            // inspect view can answer "what currently violates the
            // design?" without touching the app. Driving checks
            // (interactions/layout/behavior) are NOT run here.
            let loaded_contract = run.lock().unwrap().contract().cloned();
            let contract_violations: Vec<serde_json::Value> = match loaded_contract {
                Some(ref contract) if !contract.components.is_empty() => contract
                    .components
                    .iter()
                    .filter_map(|comp| {
                        let want = comp.role.trim().to_lowercase();
                        let component_hit = crate::semantic::detect_components(&screen)
                            .iter()
                            .any(|c| match c {
                                crate::semantic::Component::Table(_) => want == "table",
                                crate::semantic::Component::Tree(_) => want == "tree",
                                crate::semantic::Component::Scrollbar(_) => want == "scrollbar",
                            });
                        let region_hit = sem
                            .regions
                            .iter()
                            .any(|r| format!("{:?}", r.kind).to_lowercase() == want);
                        let node_hit =
                            role_matches_contract(want.as_str(), &tree.root);
                        let found = component_hit || region_hit || node_hit;
                        if found {
                            None
                        } else {
                            Some(json!({
                                "component": comp.name,
                                "role": comp.role,
                                "required": comp.required,
                                "severity": if comp.required { "error" } else { "warn" },
                                "detail": format!(
                                    "component role '{}' declared in contract but not found on this frame",
                                    comp.role
                                ),
                            }))
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            ok(json!({
                "frame": frame_record,
                "semantic_identity": crate::semantic::semantic_identity_fused(&sem, &tree),
                "viewport": { "cols": screen.cols, "rows": screen.rows },
                "title": screen.title,
                "focus": {
                    "label": sem.focus.control,
                    "id": sem.focus.control_id,
                    "confidence": sem.focus.confidence,
                },
                "controls": controls_json,
                "control_count": sem.controls.len(),
                "targeted": target.is_some(),
                "regions": sem.regions.iter().map(|r| json!({
                    "id": r.id,
                    "kind": r.kind,
                    "bounds": r.bounds,
                    "clipping": r.clipping_state,
                })).collect::<Vec<_>>(),
                "clipped_regions": clipped.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
                "components": sem.components,
                "affordances": sem.affordances,
                "native": {
                    "active": native_report.active(),
                    "adapter_status": sess.adapter_status(),
                    "framework": sess.native_channel().framework,
                    "matched": native_report.matched,
                    "native_only": native_report.native_only,
                    "ambiguous": native_report.ambiguous,
                },
                "contract": {
                    "loaded": loaded_contract.is_some(),
                    "violations": contract_violations,
                    "note": if loaded_contract.is_some() {
                        "static component-presence checks only; driving checks (interactions/layout/behavior) are tui_contract action=status"
                    } else {
                        "no contract loaded (tui_contract action=load)"
                    },
                },
            }))
        }
    }
}

/// The committed frame record for the session's current screen, when this
/// run has one (finding 36: the inspect view cites the frame id, not just
/// hashes). `None` = no committed frame (the run evicted it or the frame
/// was never committed) — the view still carries both hashes.
///
/// Beta-audit P0.11: the match is a PROVENANCE match, not a hash match
/// alone. Structure hashes collide across sessions (two sessions running
/// the same program) and across time (a screen returned to a prior
/// layout), so the candidate must come from THIS session; and among the
/// matching records the LATEST wins (highest frame id) — an app that
/// navigated A→B→A is looking at the second A, not the first. The
/// emitted citation carries session/generation so the consuming agent
/// can see whose frame it cites.
fn run_frame_of(
    sess: &crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<crate::run::RunContext>>,
) -> Option<serde_json::Value> {
    let screen = sess.last()?;
    let structure = &screen.structure_hash;
    let run = run.lock().unwrap();
    let hit = run
        .frame_hot_records()
        .filter(|r| r.session.as_deref() == Some(&sess.id))
        .filter(|r| &r.structure_hash == structure)
        .max_by_key(|r| r.frame_id)
        .map(|r| {
            json!({
                "ref": format!("frame:{}", r.frame_id),
                "session": r.session,
                "generation": r.generation,
                "semantic_identity": r.semantic_identity,
                "screen_seq": r.screen_seq,
                "committed_at": r.committed_at,
            })
        });
    drop(run);
    hit
}

/// The contract component-presence role match (conformance's own
/// `role_matches`, re-exported here so the inspect view judges the frame
/// with the SAME rule the contract checker uses — never a diverging
/// reimplementation).
fn role_matches_contract(want: &str, node: &crate::semantic::SemanticNode) -> bool {
    if node.role.slug() == want && node.confidence.score >= 0.6 {
        return true;
    }
    node.children.iter().any(|c| role_matches_contract(want, c))
}

#[cfg(test)]
mod frame_provenance_tests {
    use super::*;

    /// Beta-audit P0.11: `run_frame_of` cites by PROVENANCE, not hash
    /// alone. Three guarantees, over one committed ring:
    ///
    /// 1. a frame from a DIFFERENT session is never cited, even with an
    ///    identical structure hash (two sessions running the same
    ///    program);
    /// 2. when the same screen structure was committed more than once
    ///    (A→B→A navigation), the LATEST frame wins;
    /// 3. no match in this run ⇒ `None` (the view falls back to bare
    ///    hashes — honest absence).
    #[test]
    fn frame_citation_is_session_scoped_and_latest_wins() {
        let run = std::sync::Arc::new(std::sync::Mutex::new(crate::run::RunContext::ephemeral()));
        let screen_with = |hash: &str| {
            let mut s = crate::screen::ScreenState::new(80, 24);
            s.structure_hash = hash.to_string();
            s
        };
        let frame = |hash: &str, session: Option<&str>| {
            let mut f = crate::backend::CanonicalFrame::new(screen_with(hash), 0, 0);
            f.session_id = session.map(str::to_string);
            f
        };

        // Two sessions ran the SAME program: identical structure hash,
        // committed from sess-b then sess-a. Then sess-a navigated
        // A→B→A: a second commit of the same hash from sess-a.
        const SAME: &str = "structure:v1:same";
        let mut g = run.lock().unwrap();
        g.commit_frame(&mut frame(SAME, Some("sess-b")), Some("sess-b"))
            .expect("commit b");
        g.commit_frame(&mut frame(SAME, Some("sess-a")), Some("sess-a"))
            .expect("commit a1");
        g.commit_frame(&mut frame(SAME, Some("sess-a")), Some("sess-a"))
            .expect("commit a2");
        drop(g);

        // sess-a, back on the shared screen: its LAST observation is the
        // A-return. Seeded through the test-only observation helper.
        let mut sess_a =
            crate::session::state::Session::new("sess-a".to_string(), "python3".to_string());
        sess_a.seed_last_observation(screen_with(SAME));
        // And a twin session whose run committed nothing.
        let mut sess_c =
            crate::session::state::Session::new("sess-c".to_string(), "python3".to_string());
        sess_c.seed_last_observation(screen_with(SAME));

        // (1) sess-a cites its LATEST matching frame — never sess-b's
        // identical-hash frame, and never its own older copy.
        let cited = run_frame_of(&sess_a, &run).expect("sess-a cites a frame");
        assert_eq!(cited["ref"], "frame:3", "{cited}");
        assert_eq!(cited["session"], "sess-a", "{cited}");
        // (2) sess-b still cites its own frame, not sess-a's:
        let mut sess_b =
            crate::session::state::Session::new("sess-b".to_string(), "python3".to_string());
        sess_b.seed_last_observation(screen_with(SAME));
        let cited_b = run_frame_of(&sess_b, &run).expect("sess-b cites a frame");
        assert_eq!(cited_b["ref"], "frame:1", "{cited_b}");
        // (3) an unprovenanced session on the same screen gets NO
        // citation — honest absence, not a hash-only hit:
        assert!(run_frame_of(&sess_c, &run).is_none());
    }
}
