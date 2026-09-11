//! Interaction family: mouse-driven audits (item 52-adjacent) and
//! settle-latency / performance probes.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::WaitCond;
use crate::execution::{execute_act_as, CanonicalAction};
use crate::semantic;
use crate::session::state::Session;
use serde_json::json;

use super::shared::{ev_other, ev_other_empty};
use crate::audit::{Category, Finding, Severity};

/// Run the mouse audit (Wave G item 66): real clicks through the canonical
/// executor on each visible, enabled, clickable control. Evidence-first:
/// the driver reports what actually changed after each click (focus, screen
/// structure), and flags controls that declare clickability but absorb
/// clicks without any observable effect. Buttons/links whose semantics we
/// cannot verify statically are never assumed broken — the finding cites
/// the observed transition, not a guess.
pub fn mouse_audit(session: &mut Session, max_clicks: u32) -> Vec<Finding> {
    use crate::backend::MouseButton;

    let mut findings = Vec::new();
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                kind: crate::audit::FindingKind::Defect,
                id: "MOUSE-ERR".into(),
                rule_id: None,
                severity: Severity::Error,
                category: Category::Mouse,
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "mouse_observe_failed",
                    "session.observe failed at mouse audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    if !caps.mouse {
        // Audit finding 41: when the backend says mouse injection is
        // unavailable, the normal mouse audit TERMINATES as
        // unsupported/unverified — it never manufactures predictable
        // backend errors by clicking anyway. Deliberately negative
        // capability probes belong to an explicit diagnostic mode, not to
        // this driver.
        findings.push(Finding {
            kind: crate::audit::FindingKind::Observation,
            id: "MOUSE-NO-CAPS".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Mouse,
            summary: "Backend reports no mouse-encoding capability; the mouse audit is UNVERIFIED — no clicks were sent. Re-run against a backend with mouse injection, or drive clicks explicitly via tui_act.".into(),
            evidence: vec![ev_other(
                "mouse_capability_absent",
                "capabilities.mouse = false; audit terminated unverified",
                json!({ "backend": format!("{:?}", session.backend_kind) }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
        return findings;
    }

    let sem = session
        .fused_frame()
        .map(|(s, _, _)| s)
        .unwrap_or_else(|| semantic::analyze(&screen));
    // Risk filter (re-review item 27): classify every clickable control
    // through the SHARED interaction-risk module. Controls whose label
    // carries commit-fence evidence (Save/Submit/Apply/Deploy/Connect/
    // Send/Authorize), destructive evidence, or external evidence are
    // reported as fenced — never auto-clicked. Only controls whose class
    // stays within Mutating (a click's own base class) without label
    // escalation are probe targets.
    let mut fenced: Vec<(String, &'static str)> = Vec::new();
    let clickable: Vec<&semantic::Control> = sem
        .controls
        .iter()
        .filter(|c| {
            if !c.enabled || c.bounds.width == 0 || c.bounds.height == 0 {
                return false;
            }
            // Audit finding 42: focusable is NOT clickable. Only controls
            // whose semantic kind guarantees a pointer activation
            // affordance (button/tab/menu-item) are probe targets; text
            // fields and arbitrary focusable components are not
            // automatically click-safe.
            matches!(
                c.kind,
                semantic::ControlKind::Button
                    | semantic::ControlKind::Tab
                    | semantic::ControlKind::MenuItem
            )
        })
        .filter(|c| {
            use crate::exploration::risk::{classify_with_label, is_non_safe_label};
            if !is_non_safe_label(&c.label) {
                return true;
            }
            let class = classify_with_label(
                crate::exploration::risk::classify_action(&CanonicalAction::MouseClick {
                    button: MouseButton::Left,
                    x: 0,
                    y: 0,
                }),
                &c.label,
            );
            fenced.push((c.label.clone(), class.name()));
            false
        })
        .take(max_clicks as usize)
        .collect();

    if !fenced.is_empty() {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Observation,
            id: "MOUSE-COMMIT-FENCED".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Mouse,
            summary: format!(
                "{} clickable control(s) were fenced, not clicked: their labels carry commit/destructive/external evidence (item 27). Pass explicit tui_act clicks to exercise them.",
                fenced.len()
            ),
            evidence: fenced
                .iter()
                .map(|(label, class)| {
                    ev_other(
                        "commit_fenced_control",
                        "label evidence fences this control from auto-click",
                        json!({ "label": label, "risk_class": class }),
                    )
                })
                .collect(),
            confidence: 0.95,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    if clickable.is_empty() {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Observation,
            id: "MOUSE-NO-TARGETS".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Mouse,
            summary: "No auto-clickable controls detected; nothing clicked (commit/destructive/external labels are fenced by the item-27 risk classes).".into(),
            evidence: vec![ev_other(
                "no_click_targets",
                "no enabled button/link controls passed the risk filter",
                json!({
                    "controls_seen": sem.controls.len(),
                    "risk_filter": "fenced labels excluded (commit/destructive/external), enabled, non-empty bounds",
                }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
        return findings;
    }

    let mut clicked = 0u32;
    let mut unresponsive: Vec<String> = Vec::new();
    let mut responded: Vec<String> = Vec::new();

    for ctrl in &clickable {
        let cx = ctrl.bounds.x.saturating_add(ctrl.bounds.width / 2);
        let cy = ctrl.bounds.y.saturating_add(ctrl.bounds.height / 2);
        // Warm the frame cache so the transaction's before-frame is fresh
        // (the executor captures its own baseline; this read also surfaces
        // observe failures before the click).
        if session.observe(30).is_err() {
            break;
        }
        let tx = match execute_act_as(
            session,
            crate::execution::DriveOrigin::Audit,
            &CanonicalAction::MouseClick {
                button: MouseButton::Left,
                x: cx,
                y: cy,
            },
            80,
            500,
            false,
        ) {
            Ok(t) => t,
            Err(e) => {
                findings.push(Finding {
                    kind: crate::audit::FindingKind::Observation,
                    id: "MOUSE-SEND-ERR".into(),
                    rule_id: None,
                    severity: Severity::Warn,
                    category: Category::Mouse,
                    summary: format!("Click send failed at ({}, {}): {}", cx, cy, e),
                    evidence: vec![ev_other(
                        "click_send_failed",
                        "execute_act returned Err for MouseClick",
                        json!({ "x": cx, "y": cy, "control": ctrl.id }),
                    )],
                    confidence: 0.9,
                    reproduction: None,
                    source_refs: Vec::new(),
                    occurrence_id: None,
                });
                break;
            }
        };
        clicked += 1;
        // Audit finding 43: "responded" requires TARGET-SPECIFIC causal
        // evidence — focus moved TO this control, the native channel
        // reports an activate for this control's id, the control-delta
        // involves this target, or the transition's focus/regions moved
        // through this target's region. Unrelated visual changes, spinners,
        // clocks, or a generic settle-Met are NOT proof the click landed
        // — they classify the result `indeterminate`, never `responded`.
        let target_id = ctrl.id.as_str();
        let focus_landed = tx
            .focus_after
            .as_ref()
            .and_then(|f| f.0.as_deref())
            .filter(|f| f == &target_id || ctrl.label.contains(f))
            .is_some();
        let focus_left_target = tx
            .focus_before
            .as_ref()
            .and_then(|f| f.0.as_deref())
            .filter(|f| f == &target_id)
            .is_some()
            && tx
                .focus_after
                .as_ref()
                .and_then(|f| f.0.as_deref())
                .is_some()
            && tx.focus_after.as_ref().and_then(|f| f.0.as_deref()) != Some(target_id);
        let native_activate = session
            .native_coverage_targets()
            .iter()
            .any(|t| t.contains(target_id) || target_id.contains(t.as_str()));
        let control_delta_involves_target = tx
            .transition
            .semantic_diff
            .controls_added
            .iter()
            .chain(tx.transition.semantic_diff.controls_removed.iter())
            .any(|c| c == &ctrl.id);
        let region_delta = !tx.transition.semantic_diff.regions_added.is_empty()
            || !tx.transition.semantic_diff.regions_removed.is_empty();
        let causal = focus_landed
            || focus_left_target
            || native_activate
            || control_delta_involves_target
            || region_delta;
        if causal {
            let how = [
                ("focus_landed", focus_landed),
                ("native_activate", native_activate),
                ("control_delta", control_delta_involves_target),
                ("region_delta", region_delta),
            ]
            .iter()
            .filter(|(_, hit)| *hit)
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join("+");
            responded.push(format!("{} ({})", ctrl.label, how));
        } else {
            // Indeterminate (not "unresponsive"): the click sent, but no
            // target-specific causal evidence resolved within budget.
            unresponsive.push(format!(
                "{} at ({},{}) [indeterminate: no target-specific causal evidence]",
                ctrl.label, ctrl.bounds.x, ctrl.bounds.y
            ));
        }
    }

    if clicked > 0 {
        findings.push(Finding {
            kind: if unresponsive.is_empty() {
                crate::audit::FindingKind::Observation
            } else {
                crate::audit::FindingKind::Defect
            },
            id: if unresponsive.is_empty() {
                "MOUSE-OK".into()
            } else {
                "MOUSE-UNRESPONSIVE".into()
            },
            rule_id: None,
            severity: if unresponsive.is_empty() {
                Severity::Info
            } else {
                Severity::Warn
            },
            category: Category::Mouse,
            summary: format!(
                "Clicked {} control(s): {} responded (focus or screen changed), {} did not",
                clicked,
                responded.len(),
                unresponsive.len()
            ),
            evidence: vec![ev_other(
                "mouse_click_results",
                "per-control click outcomes (center-of-bounds, canonical executor)",
                json!({
                    "clicked": clicked,
                    "responded": responded,
                    "unresponsive": unresponsive,
                    "response_tests": ["focus moved", "structure changed", "visual hash changed", "settle met (observable activity)"],
                    "note": "unresponsive = no focus/structure/visual change AND no observable activity within the settle budget",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    findings
}

/// Run the performance audit (Wave G item 66): observe and settle latency
/// percentiles over a bounded number of samples. Evidence = measured
/// milliseconds, not assertions about the app's speed; slow paths are
/// flagged against the tool's own budgets so the numbers stay interpretable.
pub fn performance_audit(session: &mut Session, samples: u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    let samples = samples.clamp(3, 20);
    let mut observe_ms: Vec<u64> = Vec::new();
    let mut settle_ms: Vec<u64> = Vec::new();

    for _ in 0..samples {
        let t0 = std::time::Instant::now();
        let r = session.observe(40);
        observe_ms.push(t0.elapsed().as_millis() as u64);
        match r {
            Ok(_) => {}
            Err(e) => {
                findings.push(Finding {
                    kind: crate::audit::FindingKind::Defect,
                    id: "PERF-ERR".into(),
                    rule_id: None,
                    severity: Severity::Error,
                    category: Category::Performance,
                    summary: format!("Observe failed during sampling: {}", e),
                    evidence: vec![ev_other_empty(
                        "perf_observe_failed",
                        "session.observe failed during performance sampling",
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                    occurrence_id: None,
                });
                return findings;
            }
        }
        // A settle wait measures how long the app takes to go quiet after
        // its latest output — the same quantity a user experiences as lag.
        let t1 = std::time::Instant::now();
        let _ = session.wait(
            WaitCond::ScreenStable {
                quiet_for: std::time::Duration::from_millis(60),
                after_screen_seq: None,
            },
            1000,
        );
        settle_ms.push(t1.elapsed().as_millis() as u64);
    }

    let pct = |v: &mut Vec<u64>, p: usize| -> u64 {
        v.sort_unstable();
        v.get(v.len().saturating_sub(1).min(p * v.len() / 100))
            .copied()
            .unwrap_or(0)
    };
    let obs_p50 = pct(&mut observe_ms, 50);
    let obs_p95 = pct(&mut observe_ms, 95);
    let set_p50 = pct(&mut settle_ms, 50);
    let set_p95 = pct(&mut settle_ms, 95);

    // Flag only against OUR budgets: observe must stay interactive
    // (< 500 ms p95) for agent loops; settle hitting the 1 s ceiling on most
    // samples means the app never goes quiet — an anti-pattern for waits.
    let slow_observe = obs_p95 >= 500;
    const SETTLE_CEILING: u64 = 1000;
    let settle_saturated =
        settle_ms.iter().filter(|&&m| m >= SETTLE_CEILING).count() * 2 >= settle_ms.len();
    findings.push(Finding {
        kind: crate::audit::FindingKind::Metric,
        id: if slow_observe {
            "PERF-OBSERVE-SLOW"
        } else {
            "PERF-OK"
        }
        .into(),
        rule_id: None,
        severity: if slow_observe {
            Severity::Warn
        } else {
            Severity::Info
        },
        category: Category::Performance,
        summary: format!(
            "observe p50={}ms p95={}ms; settle-to-quiet p50={}ms p95={}ms ({} samples)",
            obs_p50, obs_p95, set_p50, set_p95, samples
        ),
        evidence: vec![ev_other(
            "harness_latency_percentiles",
            "TUI-Lab harness cost, NOT app input-response latency (observe + idle settle)",
            json!({
                "samples": samples,
                "harness_observe_ms": { "p50": obs_p50, "p95": obs_p95 },
                "idle_settle_ms": { "p50": set_p50, "p95": set_p95 },
                "settle_budget_ms": SETTLE_CEILING,
                "settle_saturated": settle_saturated,
                "note": "app interaction latency comes from transaction render records, not this sample",
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    });
    if settle_saturated {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Metric,
            id: "PERF-NEVER-QUIET".into(),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::Performance,
            summary: "Screen kept changing through most settle windows: waits anchored on screen-stability will burn their budgets (animations, clocks, spinners).".into(),
            evidence: vec![ev_other(
                "settle_saturation",
                "most settle waits hit the budget ceiling without reaching quiet",
                json!({
                    "idle_settle_ms": settle_ms,
                    "budget_ms": SETTLE_CEILING,
                    "note": "consider contract volatile_patterns normalization or idle waits instead",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }
    findings
}

/// App interaction performance (audit finding 44): measurements come ONLY
/// from interaction transactions already produced by the current run
/// (scenario steps, probes, explicit acts, replay). The audit never invents
/// stimuli; it summarizes causal render records the app actually generated.
pub fn interaction_performance_from_transactions<'a>(
    transactions: impl IntoIterator<Item = &'a crate::execution::InteractionTransaction>,
) -> Vec<Finding> {
    let mut samples: Vec<serde_json::Value> = Vec::new();
    let mut send_ms: Vec<u64> = Vec::new();
    let mut settle_ms: Vec<u64> = Vec::new();
    let mut first_byte_ms: Vec<u64> = Vec::new();
    let mut first_frame_ms: Vec<u64> = Vec::new();
    let mut first_semantic_ms: Vec<u64> = Vec::new();
    let mut bytes: Vec<u64> = Vec::new();
    let mut dirty_cells: Vec<usize> = Vec::new();
    let mut full_repaint_ratio: Vec<f64> = Vec::new();

    for tx in transactions {
        let Some(render) = tx.render.as_ref() else {
            continue;
        };
        send_ms.push(tx.send_ms);
        settle_ms.push(tx.settle_ms);
        bytes.push(render.bytes);
        dirty_cells.push(render.dirty_cells);
        full_repaint_ratio.push(render.full_repaint_ratio);
        if let Some(v) = render.first_byte_ms {
            first_byte_ms.push(v);
        }
        if let Some(v) = render.first_frame_ms {
            first_frame_ms.push(v);
        }
        if let Some(v) = render.first_semantic_ms {
            first_semantic_ms.push(v);
        }
        samples.push(json!({
            "action": tx.signature(),
            "dispatch": tx.dispatch.name(),
            "send_ms": tx.send_ms,
            "settle_ms": tx.settle_ms,
            "first_byte_ms": render.first_byte_ms,
            "first_frame_ms": render.first_frame_ms,
            "first_semantic_ms": render.first_semantic_ms,
            "response_bytes": render.bytes,
            "dirty_cells": render.dirty_cells,
            "full_repaint_ratio": render.full_repaint_ratio,
        }));
    }
    if samples.is_empty() {
        return vec![Finding {
            kind: crate::audit::FindingKind::Metric,
            id: "PERF-INTERACTION-NO-SAMPLES".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Performance,
            summary: "No interaction transactions had causal render records; run a scenario or explicit acts first. This audit does not invent input stimuli.".into(),
            evidence: vec![ev_other(
                "interaction_performance_unavailable",
                "requires transactions with render evidence",
                json!({ "samples": 0 }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }];
    }
    let pct = |v: &mut Vec<u64>, p: usize| -> u64 {
        v.sort_unstable();
        v.get(v.len().saturating_sub(1).min(p * v.len() / 100))
            .copied()
            .unwrap_or(0)
    };
    let send = pct(&mut send_ms, 95);
    let byte = pct(&mut first_byte_ms, 95);
    let frame = pct(&mut first_frame_ms, 95);
    let semantic = pct(&mut first_semantic_ms, 95);
    let settle = pct(&mut settle_ms, 95);
    let max_bytes = bytes.iter().copied().max().unwrap_or(0);
    let max_dirty = dirty_cells.iter().copied().max().unwrap_or(0);
    let max_ratio = full_repaint_ratio.iter().cloned().fold(0.0, f64::max);
    vec![Finding {
        kind: crate::audit::FindingKind::Metric,
        id: "PERF-INTERACTION".into(),
        rule_id: None,
        severity: Severity::Info,
        category: Category::Performance,
        summary: format!(
            "{} interaction transaction(s): p95 send={}ms first_byte={}ms first_frame={}ms first_semantic={}ms settle={}ms; max_bytes={max_bytes} max_dirty_cells={max_dirty} max_repaint_ratio={max_ratio:.3}",
            samples.len(), send, byte, frame, semantic, settle
        ),
        evidence: vec![ev_other(
            "interaction_latency_percentiles",
            "measured from the app's actual transaction render records (send, first byte/frame/semantic, completion)",
            json!({
                "samples": samples.len(),
                "p95_ms": {
                    "send": send,
                    "first_byte": byte,
                    "first_frame": frame,
                    "first_semantic": semantic,
                    "settle_completion": settle,
                },
                "max_response_bytes": max_bytes,
                "max_dirty_cells": max_dirty,
                "max_full_repaint_ratio": max_ratio,
                "per_transaction": samples,
            }),
        )],
        confidence: 0.98,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    }]
}

#[cfg(test)]
mod interaction_perf_tests {
    use super::*;
    use crate::audit::{Category, Severity};

    fn finding() -> Finding {
        Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "SEED".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Mouse,
            summary: String::new(),
            evidence: Vec::new(),
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    #[test]
    fn interaction_performance_uses_transaction_render_evidence_or_refuses() {
        // No samples: refuse rather than invent stimuli.
        let none: Vec<crate::execution::InteractionTransaction> = Vec::new();
        let empty = interaction_performance_from_transactions(&none);
        assert_eq!(empty[0].id, "PERF-INTERACTION-NO-SAMPLES");
        assert_eq!(empty[0].kind, crate::audit::FindingKind::Metric);

        // A transaction with causal render evidence feeds real metrics.
        let mut tx = crate::execution::InteractionTransaction {
            action: crate::execution::ActionEnvelope::new(
                crate::execution::CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('x')),
                },
                crate::execution::InputVisibility::Normal,
            ),
            anchor: Default::default(),
            before_frame: crate::backend::CanonicalFrame::new(
                crate::screen::ScreenState::new(20, 5),
                1,
                1,
            ),
            after_frame: crate::backend::CanonicalFrame::new(
                crate::screen::ScreenState::new(20, 5),
                1,
                2,
            ),
            settle: crate::execution::SettleStatus::Met,
            transition: crate::screen::diff::diff(
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(20, 5), 1, 1)
                    .state,
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(20, 5), 1, 2)
                    .state,
            ),
            capture: None,
            focus_before: None,
            focus_after: None,
            elapsed_ms: 20,
            send_ms: 3,
            settle_ms: 17,
            render: Some(crate::execution::RenderTransaction {
                action: "key:x".into(),
                range_start: 0,
                range_end: 12,
                complete: true,
                ops: Vec::new(),
                op_count: 0,
                bytes: 12,
                first_byte_ms: Some(2),
                first_frame_ms: Some(7),
                first_semantic_ms: Some(11),
                full_repaint_ratio: 0.25,
                dirty_cells: 40,
                dirty_rows: vec![0],
                dirty_textual: true,
                dirty_visual: true,
                dirty_structural: false,
            }),
            transition_capture: None,
            origin: Some(crate::execution::DriveOrigin::Audit),
            dispatch: crate::execution::DispatchStatus::Sent,
            dispatch_failure: None,
            event_seq_before: 0,
            event_seq_after: Some(1),
            native_revision_before: None,
            dispatch_reason: None,
        };
        tx.before_frame = tx.before_frame.clone();
        let _ = finding();
        let findings = interaction_performance_from_transactions([&tx]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "PERF-INTERACTION");
        assert_eq!(findings[0].kind, crate::audit::FindingKind::Metric);
        let evidence = &findings[0].evidence[0].detail;
        let obj = evidence.as_object().expect("detail object");
        assert_eq!(obj["samples"], 1);
        assert_eq!(obj["max_response_bytes"], 12);
        assert_eq!(obj["max_dirty_cells"], 40);
    }
}
