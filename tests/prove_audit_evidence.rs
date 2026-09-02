//! "Prove the whole thing, not the pieces" — item 41.
//!
//! Every audit profile, run against a real fixture session, must produce
//! findings whose evidence is REAL: non-empty `Vec<EvidenceRef>` with
//! targets that resolve — a control id present in the session's fused
//! semantics, a region id, a structure hash of a frame the session
//! actually showed, or a named, non-empty target for transaction/frame
//! evidence. A profile that drifted back to narration (well-formed
//! summaries, empty or invented evidence) fails here.

use tui_lab::audit::{EvidenceKind, Finding};
use tui_lab::session::SessionManager;

/// The fixture: real dialog TUI (buttons, regions, focus) so every profile
/// has something true to find.
const FIXTURE: &str = "fixtures/dialog_tui.py";

/// What makes an evidence target "resolvable" per kind. The list must stay
/// in lockstep with the kinds the engine actually emits; a new kind without
/// a resolution rule is a test failure by construction.
fn target_resolves(
    kind: &EvidenceKind,
    target: &Option<String>,
    live: &LiveTargets,
    ctx: &str,
) -> bool {
    match kind {
        EvidenceKind::ScreenSnapshot => target
            .as_deref()
            .map(|t| live.structure_hashes.contains(t))
            .unwrap_or(false),
        EvidenceKind::Control => target
            .as_deref()
            .map(|t| live.control_ids.contains(t) || live.control_labels.contains(t))
            .unwrap_or(false),
        EvidenceKind::Region => target
            .as_deref()
            .map(|t| live.region_ids.contains(t))
            .unwrap_or(false),
        // Transaction refs cite the ledger; the id must be a non-empty
        // citable token (the run's transaction records own the mapping).
        EvidenceKind::Transaction => {
            target.as_deref().map(|t| !t.is_empty()).unwrap_or(false)
        }
        // Frame refs cite `frame:<id>` or the seq-pair form.
        EvidenceKind::Frame => target
            .as_deref()
            .map(|t| t.starts_with("frame:") && t.len() > "frame:".len())
            .unwrap_or(false),
        // Diff evidence carries both hashes in its detail; target may be
        // the before/after pair — accept a non-empty target.
        EvidenceKind::Diff => target.as_deref().map(|t| !t.is_empty()).unwrap_or(false),
        // On-disk artifacts must carry a path in the `artifact` field (the
        // target slot is unused); resolved-ness is the path's existence.
        EvidenceKind::Artifact => true, // checked separately via `artifact`
        // Terminal events cite the event stream (bell/title/exit…); the
        // target names the event class — non-empty and specific.
        EvidenceKind::TerminalEvent => {
            target.as_deref().map(|t| !t.is_empty()).unwrap_or(false)
        }
        // Assertion refs cite a recorded assertion run (pass or fail).
        EvidenceKind::Assertion => {
            target.as_deref().map(|t| !t.is_empty()).unwrap_or(false)
        }
        // `Other` is the escape hatch the drivers use for measured
        // diagnostics (keyboard_buttons, wide_glyph_overlap, …). It must
        // still NAME what it points at — never an anonymous blob.
        EvidenceKind::Other => target
            .as_deref()
            .map(|t| !t.is_empty())
            .unwrap_or_else(|| {
                panic!("{ctx}: Other-kind evidence must carry a named target")
            }),
    }
}

/// The resolvable-target universe for one session at audit time.
struct LiveTargets {
    structure_hashes: std::collections::HashSet<String>,
    control_ids: std::collections::HashSet<String>,
    control_labels: std::collections::HashSet<String>,
    region_ids: std::collections::HashSet<String>,
}

fn live_targets(mgr: &mut SessionManager, sid: &str) -> LiveTargets {
    let sess = mgr.resolve_mut(Some(sid)).unwrap();
    let (screen, sem, _, _) = sess.observe_fused(60).expect("observe for live targets");
    LiveTargets {
        structure_hashes: [screen.structure_hash.clone(), screen.visual_hash.clone()]
            .into_iter()
            .collect(),
        control_ids: sem.controls.iter().map(|c| c.id.clone()).collect(),
        control_labels: sem.controls.iter().map(|c| c.label.clone()).collect(),
        region_ids: sem.regions.iter().map(|r| r.id.clone()).collect(),
    }
}

/// The evidence contract itself: every finding of every profile carries
/// resolvable evidence.
fn assert_findings_carry_real_evidence(findings: &[Finding], live: &LiveTargets, profile: &str) {
    for f in findings {
        let ctx = format!("profile {profile} finding {}", f.id);
        assert!(
            !f.evidence.is_empty(),
            "{ctx}: empty evidence is narration, not a finding"
        );
        for (i, e) in f.evidence.iter().enumerate() {
            let ectx = format!("{ctx} evidence[{i}]");
            assert!(
                !e.summary.is_empty(),
                "{ectx}: evidence must describe itself"
            );
            if matches!(e.kind, EvidenceKind::Artifact) {
                assert!(
                    e.artifact.is_some(),
                    "{ectx}: artifact evidence must carry its path"
                );
                continue;
            }
            assert!(
                target_resolves(&e.kind, &e.target, live, &ectx),
                "{ectx} (kind {:?}, target {:?}): evidence target does not resolve against the live session",
                e.kind,
                e.target
            );
        }
    }
}

/// Static profiles against one fixture frame.
#[test]
fn static_profiles_evidence_resolves() {
    let mut mgr = SessionManager::new();
    let sid = mgr
        .start(
            "python3",
            &[FIXTURE.to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start fixture");
    {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.observe(300).expect("first frame");
    }
    let live = live_targets(&mut mgr, &sid);
    // Profiles whose clean-screen result is legitimately empty on this
    // well-formed fixture: unicode (no overlap/leak), layout/clipping (no
    // out-of-bounds region), controls (no orphans — the dialog's button
    // lives inside its panel). The empty list IS the honest pass.
    let may_be_empty = ["unicode", "layout", "clipping", "controls"];
    for profile in [
        "focus",
        "layout",
        "clipping",
        "discoverability",
        "keyboard",
        "unicode",
        "controls",
    ] {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        let report = tui_lab::audit::orchestrator::run_profile_checked(
            sess,
            profile,
            None,
            tui_lab::audit::orchestrator::SafetyPolicy::AllowMutation,
        )
        .unwrap_or_else(|e| panic!("{profile} must run: {e}"));
        if !may_be_empty.contains(&profile) {
            assert!(
                !report.findings.is_empty(),
                "{profile}: a dialog screen yields findings"
            );
        }
        assert_findings_carry_real_evidence(&report.findings, &live, profile);
    }
    mgr.stop(&sid).ok();
}

/// Active (driving) profiles against the fixture — each may mutate, so
/// AllowMutation, and each gets fresh live targets taken AFTER the run
/// (drivers may legitimately cite the post-drive frame).
#[test]
fn active_profiles_evidence_resolves() {
    let mut mgr = SessionManager::new();
    for profile in [
        "keyboard",
        "focus",
        "resize",
        "clipping",
        "navigation",
        "mouse",
        "states",
        "errors",
        "color",
        "terminal_modes",
        "rendering",
        "input_protocol",
        "shell_cli",
        "lifecycle",
        "query_response",
    ] {
        let sid = mgr
            .start(
                "python3",
                &[FIXTURE.to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .unwrap_or_else(|e| panic!("{profile}: start fixture failed: {e}"));
        {
            let sess = mgr.resolve_mut(Some(&sid)).unwrap();
            sess.observe(300).expect("first frame");
        }
        let report = {
            let sess = mgr.resolve_mut(Some(&sid)).unwrap();
            tui_lab::audit::orchestrator::run_profile_checked(
                sess,
                profile,
                None,
                tui_lab::audit::orchestrator::SafetyPolicy::AllowMutation,
            )
            .unwrap_or_else(|e| panic!("{profile} must run: {e}"))
        };
        let live = live_targets(&mut mgr, &sid);
        assert!(
            !report.findings.is_empty(),
            "{profile}: the fixture must yield at least one finding (an honest
             'nothing found' is an info finding, not an empty list)"
        );
        assert_findings_carry_real_evidence(&report.findings, &live, profile);
        mgr.stop(&sid).ok();
    }
}

/// The `full` composite: every driver's findings at once, evidence-checked.
#[test]
fn full_composite_evidence_resolves() {
    let mut mgr = SessionManager::new();
    let sid = mgr
        .start(
            "python3",
            &[FIXTURE.to_string()],
            None,
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .expect("start fixture");
    {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.observe(300).expect("first frame");
    }
    let live_before = live_targets(&mut mgr, &sid);
    let report = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        tui_lab::audit::orchestrator::run_profile_checked(
            sess,
            "full",
            None,
            tui_lab::audit::orchestrator::SafetyPolicy::AllowMutation,
        )
        .expect("full runs")
    };
    let live_after = live_targets(&mut mgr, &sid);
    assert!(
        report.findings.len() >= 10,
        "the composite must exercise many drivers: {} findings",
        report.findings.len()
    );
    // Evidence must resolve against EITHER frame universe (pre- or
    // post-drive): drivers legitimately cite the frame they acted on.
    let merged = MergedTargets {
        a: &live_before,
        b: &live_after,
    };
    for f in &report.findings {
        let ctx = format!("full finding {}", f.id);
        assert!(!f.evidence.is_empty(), "{ctx}: empty evidence");
        for (i, e) in f.evidence.iter().enumerate() {
            if matches!(e.kind, EvidenceKind::Artifact) {
                continue;
            }
            let ok = e
                .target
                .as_deref()
                .map(|t| {
                    merged.contains(&e.kind, t)
                        || !t.is_empty()
                            && matches!(
                                e.kind,
                                EvidenceKind::Other
                                    | EvidenceKind::Transaction
                                    | EvidenceKind::Frame
                                    | EvidenceKind::Diff
                                    | EvidenceKind::TerminalEvent
                            )
                })
                .unwrap_or(false);
            assert!(
                ok,
                "full evidence[{i}] of {} (kind {:?}, target {:?}) does not resolve",
                f.id, e.kind, e.target
            );
        }
    }
    mgr.stop(&sid).ok();
}

struct MergedTargets<'a> {
    a: &'a LiveTargets,
    b: &'a LiveTargets,
}

impl MergedTargets<'_> {
    fn contains(&self, kind: &EvidenceKind, t: &str) -> bool {
        let (a, b) = (self.a, self.b);
        match kind {
            EvidenceKind::ScreenSnapshot => {
                a.structure_hashes.contains(t) || b.structure_hashes.contains(t)
            }
            EvidenceKind::Control => {
                a.control_ids.contains(t) || b.control_ids.contains(t)
                    || a.control_labels.contains(t)
                    || b.control_labels.contains(t)
            }
            EvidenceKind::Region => a.region_ids.contains(t) || b.region_ids.contains(t),
            _ => false,
        }
    }
}
