//! Interaction family: mouse-driven audits (item 52-adjacent) and
//! settle-latency / performance probes.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::backend::WaitCond;
use crate::execution::{execute_act, CanonicalAction};
use crate::semantic;
use crate::session::state::Session;
use serde_json::json;

use super::shared::{ev_other, ev_other_empty};
use crate::audit::Finding;

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
                id: "MOUSE-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "mouse".into(),
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "mouse_observe_failed",
                    "session.observe failed at mouse audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            return findings;
        }
    };
    let caps = session.capabilities();
    if !caps.mouse {
        findings.push(Finding {
            id: "MOUSE-NO-CAPS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "mouse".into(),
            summary: "Backend reports no mouse-encoding capability; clicks are sent but the app may never receive them.".into(),
            evidence: vec![ev_other(
                "mouse_capability_absent",
                "capabilities.mouse = false",
                json!({ "backend": session.backend_kind }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        // Continue anyway: an app with mouse support behind a
        // capability-blind backend is still worth probing honestly.
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
            matches!(
                c.kind,
                semantic::ControlKind::Button
                    | semantic::ControlKind::Tab
                    | semantic::ControlKind::MenuItem
            ) || c.focusable
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
            id: "MOUSE-COMMIT-FENCED".into(),
            rule_id: None,
            severity: "info".into(),
            category: "mouse".into(),
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
        });
    }

    if clickable.is_empty() {
        findings.push(Finding {
            id: "MOUSE-NO-TARGETS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "mouse".into(),
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
        let tx = match execute_act(
            session,
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
                    id: "MOUSE-SEND-ERR".into(),
                    rule_id: None,
                    severity: "warn".into(),
                    category: "mouse".into(),
                    summary: format!("Click send failed at ({}, {}): {}", cx, cy, e),
                    evidence: vec![ev_other(
                        "click_send_failed",
                        "execute_act returned Err for MouseClick",
                        json!({ "x": cx, "y": cy, "control": ctrl.id }),
                    )],
                    confidence: 0.9,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                break;
            }
        };
        clicked += 1;
        // Wave 4 item 38: "responded" is decided by the transaction's own
        // causal evidence, not by structure-hash equality alone. A control
        // that flashes a selection highlight (visual change, same
        // structure), moves only native focus, or produces observable
        // activity (settle Met) has RESPONDED — the old focus-or-structure
        // test labeled such legitimate responses "unresponsive".
        let focus_moved =
            tx.focus_before.and_then(|f| f.0.clone()) != tx.focus_after.and_then(|f| f.0.clone());
        let structure_changed =
            tx.transition.before_structure_hash != tx.transition.after_structure_hash;
        let visual_changed = tx.transition.before_visual_hash != tx.transition.after_visual_hash;
        let observable_activity = tx.settle == crate::execution::SettleStatus::Met;
        if focus_moved || structure_changed || visual_changed || observable_activity {
            let how = [
                ("focus", focus_moved),
                ("structure", structure_changed),
                ("visual", visual_changed),
                ("activity", observable_activity),
            ]
            .iter()
            .filter(|(_, hit)| *hit)
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join("+");
            responded.push(format!("{} ({})", ctrl.label, how));
            // Record the click edge into evidence via one finding per
            // responsive control is too noisy; they are summarized below.
        } else {
            unresponsive.push(format!(
                "{} at ({},{})",
                ctrl.label, ctrl.bounds.x, ctrl.bounds.y
            ));
        }
    }

    if clicked > 0 {
        findings.push(Finding {
            id: if unresponsive.is_empty() {
                "MOUSE-OK".into()
            } else {
                "MOUSE-UNRESPONSIVE".into()
            },
            rule_id: None,
            severity: if unresponsive.is_empty() {
                "info".into()
            } else {
                "warn".into()
            },
            category: "mouse".into(),
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
                    id: "PERF-ERR".into(),
                    rule_id: None,
                    severity: "error".into(),
                    category: "performance".into(),
                    summary: format!("Observe failed during sampling: {}", e),
                    evidence: vec![ev_other_empty(
                        "perf_observe_failed",
                        "session.observe failed during performance sampling",
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
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
        id: if slow_observe {
            "PERF-OBSERVE-SLOW"
        } else {
            "PERF-OK"
        }
        .into(),
        rule_id: None,
        severity: if slow_observe { "warn" } else { "info" }.into(),
        category: "performance".into(),
        summary: format!(
            "observe p50={}ms p95={}ms; settle-to-quiet p50={}ms p95={}ms ({} samples)",
            obs_p50, obs_p95, set_p50, set_p95, samples
        ),
        evidence: vec![ev_other(
            "latency_percentiles",
            "measured observe and settle latencies over the sampling window",
            json!({
                "samples": samples,
                "observe_ms": { "p50": obs_p50, "p95": obs_p95 },
                "settle_ms": { "p50": set_p50, "p95": set_p95 },
                "settle_budget_ms": SETTLE_CEILING,
                "settle_saturated": settle_saturated,
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
    });
    if settle_saturated {
        findings.push(Finding {
            id: "PERF-NEVER-QUIET".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "performance".into(),
            summary: "Screen kept changing through most settle windows: waits anchored on screen-stability will burn their budgets (animations, clocks, spinners).".into(),
            evidence: vec![ev_other(
                "settle_saturation",
                "most settle waits hit the budget ceiling without reaching quiet",
                json!({
                    "settle_ms": settle_ms,
                    "budget_ms": SETTLE_CEILING,
                    "note": "consider contract volatile_patterns normalization or idle waits instead",
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    findings
}
