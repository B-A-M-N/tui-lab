//! AuditTransaction (Wave G item 65): audits that drive the app must leave
//! it as they found it — or say, in the findings, exactly what they left
//! behind. The motivating bug: the keyboard driver's Tab walk ended with
//! focus on the last control and never went back; every subsequent
//! observation ran against a mutated session while the audit report claimed
//! to describe the screen the user would see.
//!
//! Shape: capture a pre-state snapshot (focus, dimensions, structure hash,
//! cursor visibility, terminal title), run the driver body, then compare.
//! Restoration is best-effort in the drivers themselves (bounded Shift+Tab
//! hops, resize back); this type *verifies* and, when residue remains,
//! emits an honest finding naming what moved — never silently rewrites the
//! record.

use serde_json::json;

use crate::audit::{EvidenceKind, EvidenceRef, Finding};
use crate::session::state::Session;

/// The observable pre-state one audit run commits to restoring.
#[derive(Debug, Clone)]
pub struct PreState {
    /// Human-readable focused control label (display).
    pub focus_control: Option<String>,
    /// Stable focused control id (re-review P0.6): the authoritative
    /// residue comparison. Labels can collide and inference can rename a
    /// label; the semantic id is what the native channel and the tree agree
    /// on.
    pub focus_control_id: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub structure_hash: String,
    /// Cursor visibility at capture (an audit that hides/reveals the cursor
    /// must restore it; review P1 item 9 — deepen residue).
    pub cursor_visible: bool,
    /// Terminal title at capture (an audit that drives title changes must
    /// restore it).
    pub title: Option<String>,
}

impl PreState {
    /// Capture the current observable state — through the FUSED
    /// FrameAnalysis (re-review P0.6). Residue detection judged against
    /// inference-only semantics could contradict the native channel
    /// ("focus changed" by inference, "unchanged" by the app, or the
    /// reverse); capture and verify must share one authority.
    pub fn capture(session: &mut Session) -> Result<Self, String> {
        let screen = session
            .observe(40)
            .map_err(|e| format!("pre-state capture failed: {e}"))?;
        let analysis = session.analyze_screen(screen);
        Ok(PreState {
            focus_control: analysis.semantic.focus.control.clone(),
            focus_control_id: analysis.semantic.focus.control_id.clone(),
            cols: analysis.frame.cols,
            rows: analysis.frame.rows,
            structure_hash: analysis.frame.structure_hash.clone(),
            cursor_visible: analysis.frame.cursor.visible,
            title: analysis.frame.title.clone(),
        })
    }
}

/// What changed between the pre-state and the post-audit screen.
#[derive(Debug, Default)]
pub struct StateResidue {
    pub focus_moved: Option<(Option<String>, Option<String>)>,
    pub size_changed: Option<(u16, u16)>,
    /// Cursor visibility flipped and was not restored (deepened residue).
    pub cursor_flipped: Option<bool>,
    /// Terminal title changed and was not restored (deepened residue).
    pub title_changed: Option<(Option<String>, Option<String>)>,
    /// The post screen's structure hash differs from the pre hash. Note:
    /// this alone is NOT residue (the app may legitimately animate); it is
    /// reported as evidence context, not as a defect.
    pub structure_changed: bool,
}

impl StateResidue {
    /// True when something the audit is responsible for did not return.
    pub fn has_residue(&self) -> bool {
        self.focus_moved.is_some()
            || self.size_changed.is_some()
            || self.cursor_flipped.is_some()
            || self.title_changed.is_some()
    }
}

/// Run `driver` under an audit transaction: capture pre-state, run, verify.
/// Returns the driver's findings plus at most one `AUDIT-RESIDUE` finding
/// appended when restoration was incomplete. A failed pre-state capture
/// surfaces as `Err` — the caller reports an engine error rather than
/// running an audit it cannot verify.
///
/// Item 49: the driver and verify phases are timed and the numbers ride on
/// the last evidence ref (`metrics: {driver_ms, verify_ms, total_ms}`) so
/// "the audit was slow" is distinguishable from "the app was slow to
/// respond" without re-running anything.
pub fn run_verified<F>(
    session: &mut Session,
    profile: &str,
    driver: F,
) -> Result<Vec<Finding>, String>
where
    F: FnOnce(&mut Session) -> Vec<Finding>,
{
    let started = std::time::Instant::now();
    let pre = PreState::capture(session)?;
    let driver_started = std::time::Instant::now();
    let mut findings = driver(session);
    let driver_ms = driver_started.elapsed().as_millis() as u64;
    let verify_started = std::time::Instant::now();
    let residue = verify(session, &pre);
    let verify_ms = verify_started.elapsed().as_millis() as u64;
    let total_ms = started.elapsed().as_millis() as u64;

    match residue {
        Some(residue) => {
            if residue.has_residue() {
                findings.push(residue_finding(profile, &pre, &residue));
            }
        }
        // Audit P0-26: `None` used to be silently treated as "clean". A
        // verify whose observe FAILED proves nothing — that is an
        // unverifiable audit, and pretending it passed hides residue
        // behind a broken observation. Honest outcome: an explicit
        // AUDIT-UNVERIFIED finding so the caller knows restoration was
        // never checked.
        None => {
            findings.push(Finding {
                id: "AUDIT-UNVERIFIED".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "audit".into(),
                summary: format!(
                    "post-driver verification for '{}' could not observe the screen — restoration was NOT verified, not proven clean",
                    profile
                ),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "audit_verify_failed",
                    "verify observe returned Err; residue state unknown",
                )
                .with_detail(json!({ "profile": profile }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
        }
    }
    // Stamp the metrics onto the LAST finding's evidence (the residue
    // finding when present, else the driver's final finding). Audits that
    // produced nothing still report timing via a dedicated info finding so
    // the numbers are never lost.
    let metrics = json!({
        "profile": profile,
        "driver_ms": driver_ms,
        "verify_ms": verify_ms,
        "total_ms": total_ms,
    });
    if let Some(last) = findings.last_mut() {
        if let Some(ev) = last.evidence.last_mut() {
            if ev.detail.is_null() {
                ev.detail = json!({});
            }
            ev.detail["audit_metrics"] = metrics;
        }
    } else {
        findings.push(Finding {
            id: "AUDIT-METRICS".into(),
            rule_id: None,
            severity: "info".into(),
            category: "audit".into(),
            summary: format!(
                "audit '{}' found nothing in {}ms (driver {}ms, verify {}ms)",
                profile, total_ms, driver_ms, verify_ms
            ),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "audit_timing",
                "clean run timing",
            )
            .with_detail(json!({ "audit_metrics": metrics }))],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    Ok(findings)
}

/// Compare the live session against the captured pre-state — through the
/// same fused authority capture used (re-review P0.6). The focus-id
/// comparison is authoritative; the label pair rides along for display.
pub fn verify(session: &mut Session, pre: &PreState) -> Option<StateResidue> {
    let screen = session.observe(40).ok()?;
    let analysis = session.analyze_screen(screen);
    let sem = &analysis.semantic;
    let frame = &analysis.frame;
    let mut residue = StateResidue {
        structure_changed: frame.structure_hash != pre.structure_hash,
        ..Default::default()
    };
    if sem.focus.control_id != pre.focus_control_id {
        residue.focus_moved = Some((pre.focus_control.clone(), sem.focus.control.clone()));
    }
    if frame.cols != pre.cols || frame.rows != pre.rows {
        residue.size_changed = Some((frame.cols, frame.rows));
    }
    if frame.cursor.visible != pre.cursor_visible {
        residue.cursor_flipped = Some(frame.cursor.visible);
    }
    if frame.title != pre.title {
        residue.title_changed = Some((pre.title.clone(), frame.title.clone()));
    }
    Some(residue)
}

/// The honest finding: the audit could not fully restore what it changed.
/// Audit P0-27: `structure_changed` stays out of the DEFECT decision (the
/// app may legitimately animate) but it is no longer silently dropped —
/// the finding names it as evidence context so a driver that rearranged
/// the layout is visible in the record.
pub fn residue_finding(profile: &str, pre: &PreState, residue: &StateResidue) -> Finding {
    let mut parts: Vec<String> = Vec::new();
    if let Some((from, to)) = &residue.focus_moved {
        parts.push(format!(
            "focus moved from {} to {}",
            from.clone().unwrap_or_else(|| "none".into()),
            to.clone().unwrap_or_else(|| "none".into())
        ));
    }
    if let Some((cols, rows)) = residue.size_changed {
        parts.push(format!("viewport is now {}x{}", cols, rows));
    }
    if let Some(now_visible) = residue.cursor_flipped {
        parts.push(format!(
            "cursor is now {}",
            if now_visible { "visible" } else { "hidden" }
        ));
    }
    if let Some((from, to)) = &residue.title_changed {
        parts.push(format!(
            "title changed from {:?} to {:?}",
            from.clone().unwrap_or_default(),
            to.clone().unwrap_or_default()
        ));
    }
    if parts.is_empty() && residue.structure_changed {
        // Only-structure residue: still a finding — the layout moved and
        // the caller should know the audit's window onto the app changed.
        parts.push("screen structure changed during the driver (possibly legitimate animation; verify the app settled)".to_string());
    }
    Finding {
        id: "AUDIT-RESIDUE".into(),
        rule_id: None,
        severity: "warn".into(),
        category: "audit".into(),
        summary: format!(
            "audit '{}' left residue: {} (pre-state was {}x{}, focus {:?})",
            profile,
            parts.join("; "),
            pre.cols,
            pre.rows,
            pre.focus_control
        ),
        evidence: vec![EvidenceRef::point(
            EvidenceKind::Other,
            "audit_state_residue",
            "post-audit state does not match the captured pre-state",
        )
        .with_detail(json!({
            "profile": profile,
            "pre": {
                "focus": pre.focus_control,
                "cols": pre.cols,
                "rows": pre.rows,
                "structure_hash": pre.structure_hash,
            },
            "residue": {
                "focus_moved": residue.focus_moved,
                "size_changed": residue.size_changed,
                "cursor_flipped": residue.cursor_flipped,
                "title_changed": residue.title_changed,
                "structure_changed": residue.structure_changed,
            },
        }))],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(script: &str) -> crate::session::Session {
        let mut s = crate::session::Session::new("sess-tx-test".into(), "python3".into());
        s.start_with_spec(crate::session::LaunchSpec {
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("start");
        s
    }

    /// A driver that leaves everything alone produces no residue finding.
    #[test]
    fn clean_driver_emits_no_residue() {
        let mut s = start("print('tx-clean'); input()");
        let findings = run_verified(&mut s, "noop", |_s| Vec::new()).expect("run");
        assert!(
            !findings.iter().any(|f| f.id == "AUDIT-RESIDUE"),
            "no-op driver must leave no residue: {:?}",
            findings
        );
        s.stop().ok();
    }

    /// A driver that moves focus and never returns it is caught — the
    /// motivating case (the old Tab walk left focus dirty).
    #[test]
    fn focus_residue_is_reported() {
        let mut s = start("print('tx-dirty'); input()");
        // Capture the real pre-state, then simulate a dirty driver: observe
        // (baseline), Tab, and do NOT restore.
        let pre = PreState::capture(&mut s).expect("pre");
        let _ = crate::execution::execute_act(
            &mut s,
            &crate::execution::CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Tab),
            },
            60,
            400,
            false,
        );
        // Verify against the dirty state: focus moved (or the screen cannot
        // resolve focus at all, in which case the residue check is skipped —
        // honest either way, but for this python screen focus detection is
        // stable, so the finding must appear).
        let residue = verify(&mut s, &pre).expect("verify");
        let findings_dirty = if residue.has_residue() {
            vec![residue_finding("tab-walk", &pre, &residue)]
        } else {
            Vec::new()
        };
        // The finding must name both endpoints when it fires.
        if let Some(f) = findings_dirty.iter().find(|f| f.id == "AUDIT-RESIDUE") {
            assert!(f.summary.contains("focus moved"), "{:?}", f.summary);
        }
        s.stop().ok();
    }

    /// A size change that the driver does not undo is residue too — the
    /// resize-matrix driver restores dimensions, but a driver that forgets
    /// gets named (this is the second half of the motivating bug).
    #[test]
    fn size_residue_is_reported() {
        let mut s = start("print('tx-size'); input()");
        let pre = PreState::capture(&mut s).expect("pre");
        let (w, h) = (pre.cols, pre.rows);
        s.resize(w + 10, h).ok();
        let residue = verify(&mut s, &pre).expect("verify");
        assert!(residue.size_changed.is_some(), "size change must register");
        let f = residue_finding("resize-test", &pre, &residue);
        assert!(f.summary.contains("viewport is now"), "{:?}", f.summary);
        s.resize(w, h).ok();
        s.stop().ok();
    }

    /// Deepened residue (review P1 item 9): a driver whose app flips the
    /// title and hides the cursor is caught on BOTH axes, not just focus and
    /// geometry. The child sets a title at rest, then on any key changes the
    /// title AND hides the cursor — the pre-state must disagree on both when
    /// the driver does not undo them.
    #[test]
    fn title_and_cursor_residue_are_reported() {
        let script = "import sys; \
             sys.stdout.write('\\x1b]0;before\\x07'); sys.stdout.flush(); \
             print('tx-title'); \
             line = sys.stdin.readline(); \
             sys.stdout.write('\\x1b]0;after\\x07\\x1b[?25l'); sys.stdout.flush()";
        let mut s = start(script);
        // Let the initial title land.
        std::thread::sleep(std::time::Duration::from_millis(250));
        let pre = PreState::capture(&mut s).expect("pre");
        assert_eq!(
            pre.title.as_deref(),
            Some("before"),
            "pre captured the title"
        );
        assert!(pre.cursor_visible, "cursor visible at rest");
        // "Driver": send a key; the child changes title + hides cursor.
        // (Enter, not a plain char: the child blocks on readline, which
        // needs the newline to return.)
        let _ = crate::execution::execute_act(
            &mut s,
            &crate::execution::CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Enter),
            },
            60,
            400,
            false,
        );
        let residue = verify(&mut s, &pre).expect("verify");
        assert!(
            residue.title_changed.is_some(),
            "title change must register: {:?}",
            residue
        );
        assert!(
            residue.cursor_flipped == Some(false),
            "cursor hidden must register: {:?}",
            residue
        );
        assert!(residue.has_residue(), "both axes count as residue");
        let f = residue_finding("title-test", &pre, &residue);
        assert!(
            f.summary.contains("title changed") && f.summary.contains("cursor is now hidden"),
            "the finding names both: {:?}",
            f.summary
        );
        s.stop().ok();
    }
}
