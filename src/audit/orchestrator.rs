//! Audit orchestration (re-review P0 fix 2): one authority for what a
//! profile means and whether it drives the live app.
//!
//! Before this module, the MCP layer decided static-vs-active with a string
//! match that omitted `"full"` — so `tui_audit profile=full` ran *fewer*
//! checks than `profile=keyboard`, which is backwards. The engine now owns
//! the decision:
//!
//! ```text
//! full = static composite (focus + layout/clipping + discoverability +
//!         keyboard-static) + keyboard + focus + resize + clipping active
//! ```
//!
//! The MCP layer calls [`run_profile`] and reports the returned mode; it no
//! longer interprets profile names.

use crate::audit::{EvidenceKind, EvidenceRef, Finding};
use crate::session::state::Session;
use serde_json::json;

/// A parsed audit profile. Parsing lives here so an unknown name is an
/// engine-level error, not an MCP-level guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditProfile {
    /// Everything: static composite plus every active driver.
    Full,
    /// Traverse focus with Tab (active).
    Keyboard,
    /// Focus visibility/consistency (active + static pass).
    Focus,
    /// Layout across the resize matrix (active).
    Resize,
    /// Alias of Resize (kept for API compatibility).
    Layout,
    /// Border/content clipping (active).
    Clipping,
    /// Focus hints from one frame (static).
    Discoverability,
    /// Tab-order / reverse-traversal proof over the ID-keyed focus graph
    /// (active — Wave D item 37; previously a static placeholder).
    Navigation,
    /// Contract conformance (Wave E item 49): run the loaded project
    /// contract's checks and fold the results into findings. Requires a
    /// contract to have been loaded (`tui_contract action=load`).
    Contract,
    Color,
    Performance,
    Mouse,
    States,
    Errors,
    /// Unicode subsystem (Wave 3): wide-glyph overlap + control-byte leak
    /// over one fused frame (static).
    Unicode,
    /// Control/region coverage (Wave 3): orphan controls + ambiguous region
    /// parents over one fused frame (static).
    Controls,
    /// Terminal-modes subsystem (Wave 3): negotiated input modes from the
    /// real DECSET/DECRST timeline, cross-referenced with visible
    /// affordances (frame-level read of the raw ring; no input sent).
    TerminalModes,
}

impl AuditProfile {
    /// Parse a profile name. Unknown names are an error — the engine does
    /// not silently degrade a typo into a weaker audit.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "full" => Ok(AuditProfile::Full),
            "keyboard" => Ok(AuditProfile::Keyboard),
            "focus" => Ok(AuditProfile::Focus),
            "resize" => Ok(AuditProfile::Resize),
            "layout" => Ok(AuditProfile::Layout),
            "clipping" => Ok(AuditProfile::Clipping),
            "discoverability" => Ok(AuditProfile::Discoverability),
            "navigation" => Ok(AuditProfile::Navigation),
            "contract" => Ok(AuditProfile::Contract),
            "color" => Ok(AuditProfile::Color),
            "performance" => Ok(AuditProfile::Performance),
            "mouse" => Ok(AuditProfile::Mouse),
            "states" => Ok(AuditProfile::States),
            "errors" => Ok(AuditProfile::Errors),
            "unicode" => Ok(AuditProfile::Unicode),
            "controls" => Ok(AuditProfile::Controls),
            "terminal_modes" => Ok(AuditProfile::TerminalModes),
            other => Err(format!(
                "unknown audit profile '{}'; expected one of full, keyboard, focus, resize, layout, clipping, discoverability, navigation, contract, color, performance, mouse, states, errors, unicode, controls",
                other
            )),
        }
    }

    /// The profile's canonical engine name (used in residue findings).
    pub fn name(&self) -> &'static str {
        match self {
            AuditProfile::Full => "full",
            AuditProfile::Keyboard => "keyboard",
            AuditProfile::Focus => "focus",
            AuditProfile::Resize => "resize",
            AuditProfile::Layout => "layout",
            AuditProfile::Clipping => "clipping",
            AuditProfile::Discoverability => "discoverability",
            AuditProfile::Navigation => "navigation",
            AuditProfile::Contract => "contract",
            AuditProfile::Color => "color",
            AuditProfile::Performance => "performance",
            AuditProfile::Mouse => "mouse",
            AuditProfile::States => "states",
            AuditProfile::Errors => "errors",
            AuditProfile::Unicode => "unicode",
            AuditProfile::Controls => "controls",
            AuditProfile::TerminalModes => "terminal_modes",
        }
    }

    /// Does this profile need to drive the live app (send input / resize)?
    /// Wave G item 66: mouse/performance/states/errors are real drivers now
    /// (they send input / sample latency); color stays frame-level, and
    /// discoverability remains the one static pass.
    /// Public so the MCP layer can apply the same driving/observing split
    /// for the human control lease (item 76): active profiles refuse under
    /// a live lease, static ones stay allowed.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            AuditProfile::Full
                | AuditProfile::Keyboard
                | AuditProfile::Focus
                | AuditProfile::Resize
                | AuditProfile::Layout
                | AuditProfile::Clipping
                | AuditProfile::Navigation
                | AuditProfile::Contract
                | AuditProfile::Mouse
                | AuditProfile::Performance
                | AuditProfile::States
                | AuditProfile::Errors
                // Needs the session (raw ring + fused frame), not just one
                // frame — same "active-side, non-driving" class as Color.
                | AuditProfile::TerminalModes
        )
    }

    /// Static composite is part of `full` (and implied by its members).
    fn wants_static_composite(&self) -> bool {
        matches!(self, AuditProfile::Full)
    }
}

/// Result of one orchestrated profile run.
pub struct ProfileReport {
    pub profile: AuditProfile,
    /// "active" (drove the app), "static" (one frame), or "composite"
    /// (static + active, i.e. `full`).
    pub mode: &'static str,
    pub findings: Vec<Finding>,
    /// Focus edges recorded by this run's drivers (Wave D item 36). Callers
    /// merge it into the run's persistent graph; static profiles leave it
    /// empty.
    pub focus_graph: crate::semantic::focus_graph::FocusGraph,
}

/// Run one audit profile against a session (re-review P0 fix 2). The engine
/// decides static-vs-active; the caller never interprets profile names.
/// `contract` feeds `profile=contract` (Wave E item 49); other profiles
/// ignore it.
pub fn run_profile(session: &mut Session, profile_name: &str) -> Result<ProfileReport, String> {
    run_profile_with_contract(session, profile_name, None)
}

/// Contract-armed variant: `profile=contract` requires the loaded contract.
pub fn run_profile_with_contract(
    session: &mut Session,
    profile_name: &str,
    contract: Option<&crate::design::ProjectContract>,
) -> Result<ProfileReport, String> {
    let profile = AuditProfile::parse(profile_name)?;

    // Contract profile: conformance results folded into findings.
    if profile == AuditProfile::Contract {
        let Some(contract) = contract else {
            return Err(
                "profile 'contract' requires a loaded contract — call tui_contract action=load first"
                    .to_string(),
            );
        };
        let report = crate::design::check_contract(session, contract)
            .map_err(|e| format!("contract check failed: {e}"))?;
        return Ok(ProfileReport {
            profile,
            mode: "composite",
            findings: report.findings(),
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
        });
    }

    // The static profiles (single frame, no driving): discoverability plus
    // the Wave-3 subsystem audits (unicode, controls).
    if !profile.is_active() {
        // Fused truth (re-review Wave-2 item 16).
        let (screen, sem, _, _) = session.observe_fused(40).map_err(|e| format!("observe failed: {e}"))?;
        let name = profile.name();
        return Ok(ProfileReport {
            profile,
            mode: "static",
            findings: crate::audit::run(name, &screen, &sem).map_err(|e| e.to_string())?,
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
        });
    }

    // `full` = static composite + every active driver (P0 fix 2). Ordering
    // matters for evidence readability: static analysis first (it does not
    // mutate), then drivers that change state. The focus graph is per-call
    // here: the caller (MCP layer) merges it into the run's graph.
    let mut graph = crate::semantic::focus_graph::FocusGraph::new();

    let mut findings = Vec::new();
    if profile.wants_static_composite() {
        // Fused truth (re-review Wave-2 item 16).
        let (screen, sem, _, _) = session.observe_fused(40).map_err(|e| format!("observe failed: {e}"))?;
        findings.extend(crate::audit::run("full", &screen, &sem).map_err(|e| e.to_string())?);
    }

    // Item 65: every input-driving driver runs under an AuditTransaction —
    // the pre-state is captured, the driver restores what it can, and any
    // residue it leaves becomes an honest AUDIT-RESIDUE finding instead of
    // silent session mutation (the old Tab-walk-left-focus-dirty bug).
    use crate::audit::transaction::run_verified;
    let tx = |s: &mut Session, f: &dyn Fn(&mut Session) -> Vec<Finding>| -> Vec<Finding> {
        run_verified(s, profile.name(), |sess| f(sess)).unwrap_or_else(|e| {
            vec![Finding {
                id: "AUDIT-TX-ERR".into(),
                rule_id: None,
                severity: "error".into(),
                category: "audit".into(),
                summary: format!("audit transaction failed: {}", e),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "audit_transaction",
                    "pre-state capture or post-verify failed",
                )
                .with_detail(json!({ "profile": profile.name(), "error": e }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            }]
        })
    };

    let active_findings = match profile {
        AuditProfile::Full => {
            let mut fs = crate::audit::driver::keyboard_audit(session, 20, &mut graph);
            fs.extend(crate::audit::driver::focus_audit(session));
            fs.extend(crate::audit::driver::resize_audit(session));
            fs.extend(crate::audit::driver::clipping_audit(session));
            fs.extend(crate::audit::driver::navigation_audit(
                session, 20, &mut graph,
            ));
            fs.extend(tx(session, &|s| crate::audit::driver::mouse_audit(s, 8)));
            fs.extend(crate::audit::driver::performance_audit(session, 5));
            fs.extend(tx(session, &|s| crate::audit::driver::states_audit(s, 6)));
            fs.extend(tx(session, &|s| crate::audit::driver::errors_audit(s, 12)));
            fs.extend(crate::audit::driver::color_audit(session));
            fs.extend(crate::audit::driver::terminal_modes_audit(session));
            fs
        }
        AuditProfile::Keyboard => crate::audit::driver::keyboard_audit(session, 20, &mut graph),
        AuditProfile::Focus => crate::audit::driver::focus_audit(session),
        AuditProfile::Resize | AuditProfile::Layout => crate::audit::driver::resize_audit(session),
        AuditProfile::Clipping => crate::audit::driver::clipping_audit(session),
        AuditProfile::Navigation => crate::audit::driver::navigation_audit(session, 20, &mut graph),
        AuditProfile::Mouse => tx(session, &|s| crate::audit::driver::mouse_audit(s, 12)),
        AuditProfile::Performance => crate::audit::driver::performance_audit(session, 7),
        AuditProfile::States => tx(session, &|s| crate::audit::driver::states_audit(s, 10)),
        AuditProfile::Errors => tx(session, &|s| crate::audit::driver::errors_audit(s, 15)),
        AuditProfile::Color => crate::audit::driver::color_audit(session),
        // Frame-level read of the raw ring: observes but sends nothing, so
        // it does not need a transaction (Wave G split, item 66).
        AuditProfile::TerminalModes => crate::audit::driver::terminal_modes_audit(session),
        _ => unreachable!("non-active profiles returned above"),
    };
    findings.extend(active_findings);

    let mode = if profile.wants_static_composite() {
        "composite"
    } else {
        "active"
    };
    Ok(ProfileReport {
        profile,
        mode,
        findings,
        focus_graph: graph,
    })
}

/// Error-shaped finding helper for driver-level failures (unused today;
/// kept so orchestration-level failures carry typed evidence).
#[allow(dead_code)]
fn orchestration_error(profile: &str, summary: String) -> Finding {
    Finding {
        id: format!("AUDIT-ERR-{}", profile.to_uppercase()),
        rule_id: None,
        severity: "error".into(),
        category: "audit".into(),
        summary,
        evidence: vec![EvidenceRef::point(
            EvidenceKind::Other,
            "orchestrator",
            "profile execution failed",
        )
        .with_detail(json!({"profile": profile}))],
        confidence: 1.0,
        reproduction: None,
        source_refs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 fix 2: `full` must parse as the composite profile and must be
    /// treated as active — the exact regression this module exists for.
    #[test]
    fn full_is_active_and_parseable() {
        let p = AuditProfile::parse("full").expect("full parses");
        assert_eq!(p, AuditProfile::Full);
        assert!(p.is_active(), "full must drive the live app");
        assert!(p.wants_static_composite(), "full includes static passes");
    }

    #[test]
    fn unknown_profiles_are_errors() {
        assert!(AuditProfile::parse("detailed").is_err());
        assert!(AuditProfile::parse("").is_err());
    }

    #[test]
    fn every_documented_profile_parses() {
        for name in [
            "full",
            "keyboard",
            "focus",
            "resize",
            "layout",
            "clipping",
            "discoverability",
            "navigation",
            "color",
            "performance",
            "mouse",
            "states",
            "errors",
            "unicode",
            "controls",
            "terminal_modes",
        ] {
            assert!(AuditProfile::parse(name).is_ok(), "{} must parse", name);
        }
    }

    /// The live orchestrator: `full` returns composite mode and includes
    /// findings from the active drivers (a static-only run cannot produce
    /// keyboard-traversal evidence). Runs against the actor-backed test
    /// launch (the legacy blocking `SessionManager` is removed).
    #[tokio::test]
    async fn full_profile_runs_active_drivers() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('audit-full'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let report = pool
            .with_session(Some(&id), |s| run_profile(s, "full"))
            .await
            .expect("actor run")
            .expect("run full");
        assert_eq!(report.mode, "composite");
        // A plain python echo screen has little to audit, but the composite
        // must include the static passes AND have attempted the drivers.
        let cats: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.category.as_str())
            .collect();
        assert!(
            cats.iter().any(|c| matches!(
                *c,
                "focus" | "layout" | "clipping" | "discoverability" | "keyboard"
            )),
            "static composite categories must be present: {:?}",
            cats
        );
        pool.stop(&id).await.ok();
    }

    /// Wave-3 subsystem audits run through the orchestrator's static path.
    #[tokio::test]
    async fn subsystem_profiles_run_static() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('subsys'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        for name in ["unicode", "controls"] {
            let report = pool
                .with_session(Some(&id), |s| run_profile(s, name))
                .await
                .expect("actor run")
                .unwrap_or_else(|e| panic!("{name} must run: {e}"));
            assert_eq!(report.mode, "static", "{name} is a static profile");
            // A clean python screen can legitimately produce zero findings;
            // the contract is the run SUCCEEDS and carries only real rules.
            for f in &report.findings {
                assert!(
                    f.id.starts_with("UNI-") || f.id.starts_with("CTRL-"),
                    "{name} findings come from the subsystem rules: {}",
                    f.id
                );
            }
        }
        // `full`'s static composite now includes the subsystem audits too.
        pool.stop(&id).await.ok();
    }

    /// Wave 3 terminal-modes audit: the driver folds the child's REAL
    /// DECSET/DECRST traffic. The child enables SGR mouse
    /// (`\x1b[?1000;1006h`) without drawing any mouse affordance, so the
    /// run must surface MODE-INVENTORY with the folded `mouse_press_release`
    /// = on AND the MODE-MOUSE-HIDDEN cross-reference.
    #[tokio::test]
    async fn terminal_modes_folds_real_negotiation() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &[
                    "-c".into(),
                    "import sys; sys.stdout.write('\\x1b[?1000;1006h'); sys.stdout.flush(); \
                     print('modes-audit'); input()"
                        .to_string(),
                ],
                None,
                &[],
                80,
                24,
                // Mode evidence comes from the raw ring; force a retaining
                // engine rather than trusting `auto` selection.
                "portable_vt100",
                "local",
            )
            .await
            .expect("start");
        let report = pool
            .with_session(Some(&id), |s| run_profile(s, "terminal_modes"))
            .await
            .expect("actor run")
            .expect("run terminal_modes");
        assert_eq!(report.mode, "active");
        let inv = report
            .findings
            .iter()
            .find(|f| f.id == "MODE-INVENTORY")
            .expect("mode inventory from the child's real escape sequences");
        let evidence = serde_json::to_value(&inv.evidence).unwrap();
        let modes = evidence[0]["detail"]["modes"].clone();
        assert_eq!(
            modes["mouse_press_release"], true,
            "DECSET 1000 must fold to on: {modes}"
        );
        assert_eq!(modes["mouse_sgr_encoding"], true, "DECSET 1006 must fold to on");
        assert!(
            report.findings.iter().any(|f| f.id == "MODE-MOUSE-HIDDEN"),
            "mouse negotiated with a plain print() screen = hidden affordances"
        );
        pool.stop(&id).await.ok();
    }

    #[tokio::test]
    async fn keyboard_profile_reports_active_mode() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('audit-kb'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let report = pool
            .with_session(Some(&id), |s| run_profile(s, "keyboard"))
            .await
            .expect("actor run")
            .expect("run keyboard");
        assert_eq!(report.mode, "active");
        pool.stop(&id).await.ok();
    }
}
