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
    // Audit P0-2/P0-3: the stimulus keeps its own guard AND its own
    // sensitivity policy — a probed `sensitive: true` payload must be
    // redacted exactly where `tui_act` would redact it.
    let probe_guard = p.stimulus.as_ref().and_then(|s| s.guard());
    let visibility = p
        .stimulus
        .as_ref()
        .map(|s| s.visibility())
        .unwrap_or(crate::execution::InputVisibility::Normal);
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
    // The moved-into-the-probe action's shape, captured up front: the
    // sensitive-capture decision below needs the payload field (Type/Paste)
    // and length after `stimulus` itself has been consumed.
    let stim_shape = stimulus.as_ref().map(|a| (a.name(), a.payload_len()));
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
            // Audit P0-2: a stimulus that sends input IS machine driving.
            // The human control lease refuses it exactly like tui_act —
            // leasing the terminal means no key reaches it from any tool.
            // Drift probes (stimulus none / {kind:none}) stay observational
            // and remain allowed under a lease.
            let drives = stimulus.is_some();
            if drives {
                if let Some(refused) = crate::mcp::helpers::lease_refused(sess) {
                    return refused;
                }
            }
            // Item 13: the duration-sample capture needs its delay AFTER the
            // settle; fold it into the effective budget so the wait inside
            // the probe accounts for it.
            let effective_budget = match frame_capture {
                Some((_, extra_ms)) if extra_ms > 0 => budget_ms.saturating_add(extra_ms),
                _ => budget_ms,
            };
            // Audit P0-16: the capture spec rides INTO the probe — the
            // transition collector arms at the stimulus, not after settle.
            match crate::diagnostic::run_probe_with_guard(
                sess,
                stimulus,
                completion,
                &watch,
                quiet_ms,
                effective_budget,
                probe_guard.as_ref(),
                visibility,
                frame_capture,
            ) {
                Ok(result) => {
                    {
                        let (sid, gen) = (sess.id.clone(), sess.generation);
                        let mut run = run.lock().unwrap();
                        let _ = run.record_event(&sid, "probe");
                        // Audit P0-17: a probe is not a `wait` step. The old
                        // recording stuffed the whole TuiProbeParams into a
                        // wait step — a grammar the replay runner parses as
                        // an empty screen_stable wait, silently dropping the
                        // stimulus. Truthful evidence instead: the run event
                        // above carries the probe; scenario recordings get
                        // the DECOMPOSITION — the stimulus as an act step
                        // (replayable via the normal act path) when one was
                        // sent. The settle/observe half is the probe's own
                        // semantics and is not a replayable wait.
                        if drives {
                            if let Some(stim) = p.stimulus.as_ref() {
                                let mut params = match stim {
                                    crate::mcp::params::ProbeStimulus::Canonical(req) => {
                                        serde_json::to_value(req).unwrap_or_default()
                                    }
                                    crate::mcp::params::ProbeStimulus::Legacy(legacy) => {
                                        match legacy.to_action() {
                                            Some(a) => super::drive::act_request_json(&a),
                                            None => serde_json::json!({}),
                                        }
                                    }
                                };
                                if let serde_json::Value::Object(ref mut m) = params {
                                    m.insert("from_probe".into(), serde_json::Value::Bool(true));
                                }
                                // Audit P0-3 (E2E-verified): a sensitive
                                // stimulus records like a sensitive act —
                                // the payload field becomes a ${PARAM}
                                // reference and the secret never reaches the
                                // scenario file. The old path serialized the
                                // whole request verbatim, leaking the value.
                                let sensitive = stim.visibility()
                                    == crate::execution::InputVisibility::Sensitive;
                                let payload_field = if sensitive {
                                    match stim_shape.map(|(n, _)| n) {
                                        Some("type") => Some("text"),
                                        Some("paste") => Some("paste"),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                match payload_field {
                                    Some(field) => {
                                        let byte_len =
                                            stim_shape.map(|(_, l)| l).unwrap_or_default();
                                        let _ = run.record_scenario_act_sensitive(
                                            &sid,
                                            gen,
                                            params,
                                            field,
                                            crate::scenario::model::SensitiveKind::Secret,
                                            byte_len,
                                        );
                                    }
                                    None if sensitive => {
                                        if let serde_json::Value::Object(ref mut m) = params {
                                            m.insert(
                                                "redacted".into(),
                                                serde_json::Value::Bool(true),
                                            );
                                        }
                                        let _ = run.record_scenario_act(&sid, gen, params);
                                    }
                                    None => {
                                        let _ = run.record_scenario_act(&sid, gen, params);
                                    }
                                }
                            }
                        }
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
                            "controls_changed": result.transition.semantic_diff.controls_changed,
                            // Finding 40: semantic render deltas — per
                            // control WHAT changed ("button/save moved
                            // x:65→71"), not just a raw cell count.
                            "control_deltas": &result.transition.semantic_diff.control_deltas,
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
                    // Item 13 + audit P0-16: the transition capture rode INTO
                    // the probe — the collector armed at the stimulus, so
                    // these are the redraw/flicker frames of the transition
                    // itself, not the post-settle plateau the old code
                    // photographed.
                    if let Some(cap) = &result.transition_capture {
                        result_json["capture"] = serde_json::to_value(cap).unwrap_or_default();
                    }
                    ok(result_json)
                }
                Err(e) => err(ErrorCategory::BackendError, e.to_string()),
            }
        })
        .await
        .unwrap_or_else(|e| e)
}
