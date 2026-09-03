//! tui_probe: one controlled experiment with baseline/stimulus/settle.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_probe` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_probe(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiProbeParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    // Completion: known names compile to the canonical policy; unknown
    // names are answered with the accepted list.
    let completion = match p.completion.as_ref() {
        None => crate::capture::CompletionPolicy::StableScreen,
        Some(Known::Known(pc)) => match pc.to_policy(p.text.as_deref()) {
            Some(c) => c,
            None => {
                return err(
                    ErrorCategory::InvalidRequest,
                    format!("completion '{}' requires the 'text' parameter", pc.as_str()),
                )
            }
        },
        Some(Known::Other(other)) => {
            return err(
                ErrorCategory::InvalidRequest,
                format!(
                    "unknown completion '{}' (expected one of: {})",
                    other,
                    crate::mcp::params::ProbeCompletion::VARIANTS.join(", ")
                ),
            )
        }
    };
    let watch: Vec<crate::diagnostic::ProbeWatch> = match &p.watch {
        None => crate::diagnostic::default_watch(),
        Some(sel) => {
            let mut out = Vec::new();
            for w in sel {
                match w {
                    Known::Known(w) => out.push(w.to_watch()),
                    Known::Other(other) => {
                        return err(
                            ErrorCategory::InvalidRequest,
                            format!(
                                "unknown watch aspect '{}' (expected one of: {})",
                                other,
                                crate::mcp::params::ProbeWatchParam::VARIANTS.join(", ")
                            ),
                        )
                    }
                }
            }
            out
        }
    };
    // Re-review item 12: the stimulus is the canonical action grammar —
    // the exact shapes tui_act takes — so the probe can apply anything
    // the act tool can (paste, raw bytes, mouse families, resize,
    // signal), not just the old key/type/click trio. Drift probes: no
    // stimulus, or the legacy {"kind":"none"}.
    let probe_guard = p.stimulus.as_ref().and_then(|s| s.guard());
    let stimulus = match &p.stimulus {
            None => None,
            // A canonical request ALWAYS names a real action; the legacy
            // `{"kind":"none"}` is the drift probe. Distinguish them by
            // shape so a drift probe is not misreported as an unrecognized
            // stimulus.
            Some(crate::mcp::params::ProbeStimulus::Legacy(
                crate::mcp::params::LegacyStimulus::None,
            )) => None,
            Some(s) => match s.to_action() {
                Some(a) => Some(a),
                None => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "unrecognized stimulus: send a canonical action ({action:...}) or legacy {kind:none} for a drift probe",
                    )
                }
            },
        };
    // Re-review item 13: the structured capture spec — "the first N
    // frames of the transition" and "sample after a fixed delay" are
    // capture questions the completion names cannot ask. Handled as a
    // wrapper around the standard probe (the settle policy still runs
    // so `after` is decided, then the extra frames ride along).
    let frame_capture = p.capture.as_ref().map(|c| match c {
        crate::mcp::params::ProbeCapture::Frames { count } => (*count, 0u64),
        crate::mcp::params::ProbeCapture::AfterDuration { ms } => (0usize, *ms),
    });
    let quiet_ms = p.quiet_ms.unwrap_or(120);
    let budget_ms = p.budget_ms.unwrap_or(5000);
    let selector = p.id.clone();
    let run = s.run.clone();
    s.with_sess(selector.as_deref(), move |sess| {
            // Item 13: the duration-sample capture needs its delay AFTER the
            // settle; fold it into the effective budget so the wait inside
            // the probe accounts for it.
            let effective_budget = match frame_capture {
                Some((_, extra_ms)) if extra_ms > 0 => budget_ms.saturating_add(extra_ms),
                _ => budget_ms,
            };
            match crate::diagnostic::run_probe_with_guard(
                sess,
                stimulus,
                completion,
                &watch,
                quiet_ms,
                effective_budget,
                probe_guard.as_ref(),
            ) {
                Ok(result) => {
                    {
                        let (sid, gen) = (sess.id.clone(), sess.generation);
                        let mut run = run.lock().unwrap();
                        let _ = run.record_event(&sid, "probe");
                        let _ = run.record_scenario_wait(
                            &sid,
                            gen,
                            serde_json::to_value(&p).unwrap_or_default(),
                        );
                    }
                    let events = result
                        .terminal_events
                        .iter()
                        .map(|ev| {
                            json!({
                                "seq": ev.seq,
                                "kind": ev.kind.name(),
                                "detail": format!("{:?}", ev.kind),
                            })
                        })
                        .collect::<Vec<_>>();
                    let material_changes = result.material_changes.clone();
                    let mut result_json = json!({
                        "action": result.action,
                        "settle": format!("{:?}", result.settle),
                        "changed": result.has_changes(),
                        "timing_ms": result.timing_ms,
                        "events": events,
                        "material_changes": material_changes,
                        "transition": {
                            "structure_changed": result.transition.before_structure_hash != result.transition.after_structure_hash,
                            "changed_cells": result.transition.screen_diff.changed_cells,
                            "style_changes": result.transition.screen_diff.style_changes,
                            "controls_added": result.transition.semantic_diff.controls_added,
                            "controls_removed": result.transition.semantic_diff.controls_removed,
                            "focus_before": result.transition.semantic_diff.focus_before,
                            "focus_after": result.transition.semantic_diff.focus_after,
                        },
                        "before": {
                            "structure_hash": result.before.structure_hash,
                            "visual_hash": result.before.visual_hash,
                        },
                        "after": {
                            "structure_hash": result.after.structure_hash,
                            "visual_hash": result.after.visual_hash,
                            "viewport_text": result.after.viewport_text,
                            "focus": {
                                "control_id": result.after_focus.as_ref().and_then(|f| f.0.clone()),
                                "label": result.after_focus.as_ref().and_then(|f| f.1.clone()),
                                "cursor": { "x": result.after.cursor.x, "y": result.after.cursor.y, "visible": result.after.cursor.visible },
                            },
                            "process": {
                                "running": result.after.process.running,
                                "exit_code": result.after.process.exit_code,
                            },
                        },
                        "frames_captured": result.frames.len(),
                    });
                    // Item 13: a frames:N capture — the first N distinct
                    // frames AFTER the settled state (the microscope view
                    // of a redraw, animation, or spin loop). Reported
                    // separately from the probe's own settle frames so the
                    // completion evidence stays clean; an AfterDuration
                    // capture samples one frame after a fixed delay.
                    if let Some(spec) = frame_capture {
                        let capture_json = match spec {
                            (count, _) if count > 0 => {
                                let anchor = sess.event_state().screen_seq;
                                let outcome = crate::capture::capture_frame_sequence(
                                    sess.backend_mut(),
                                    count,
                                    anchor,
                                    std::time::Duration::from_millis(budget_ms),
                                );
                                json!({
                                    "requested": outcome.requested,
                                    "captured": outcome.captured,
                                    "completed": outcome.completed,
                                    "reason": outcome.reason.name(),
                                    "elapsed_ms": outcome.elapsed_ms,
                                    "frames": outcome.frames.iter().map(|f| json!({
                                        "structure_hash": f.structure_hash,
                                        "visual_hash": f.visual_hash,
                                        "viewport_text": f.viewport_text,
                                    })).collect::<Vec<_>>(),
                                })
                            }
                            (_, delay_ms) => {
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(5000)));
                                match sess.observe(30) {
                                    Ok(f) => json!({
                                        "sampled_after_ms": delay_ms,
                                        "frame": {
                                            "structure_hash": f.structure_hash,
                                            "visual_hash": f.visual_hash,
                                            "viewport_text": f.viewport_text,
                                        },
                                    }),
                                    Err(e) => json!({ "error": format!("delayed sample failed: {e}") }),
                                }
                            }
                        };
                        result_json["capture"] = capture_json;
                    }
                    ok(result_json)
                }
                Err(e) => err(ErrorCategory::BackendError, e.to_string()),
            }
        })
        .await
        .unwrap_or_else(|e| e)
}
