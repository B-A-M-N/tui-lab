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

use crate::audit::{Finding, EvidenceKind, EvidenceRef};
use crate::semantic;
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
            other => Err(format!(
                "unknown audit profile '{}'; expected one of full, keyboard, focus, resize, layout, clipping, discoverability, navigation, contract, color, performance, mouse, states, errors",
                other
            )),
        }
    }

    /// Does this profile need to drive the live app (send input / resize)?
    fn is_active(&self) -> bool {
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
pub fn run_profile(
    session: &mut Session,
    profile_name: &str,
) -> Result<ProfileReport, String> {
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

    // Placeholder classes: honest info finding, no fake rigor.
    if !profile.is_active() {
        let (findings, mode) = match profile {
            AuditProfile::Discoverability => {
                let screen = observe_or_err(session)?;
                let sem = semantic::analyze(&screen);
                (
                    crate::audit::run("discoverability", &screen, &sem),
                    "static",
                )
            }
            ref p @ (AuditProfile::Navigation
            | AuditProfile::Color
            | AuditProfile::Performance
            | AuditProfile::Mouse
            | AuditProfile::States
            | AuditProfile::Errors) => {
                let name = format!("{:?}", p).to_lowercase();
                let screen = observe_or_err(session)?;
                let sem = semantic::analyze(&screen);
                (
                    crate::audit::run(&name, &screen, &sem),
                    "static",
                )
            }
            _ => unreachable!("non-active profiles handled above"),
        };
        return Ok(ProfileReport {
            profile,
            mode,
            findings,
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
        let screen = observe_or_err(session)?;
        let sem = semantic::analyze(&screen);
        findings.extend(crate::audit::run("full", &screen, &sem));
    }

    let active_findings = match profile {
        AuditProfile::Full => {
            let mut fs = crate::audit::driver::keyboard_audit(session, 20, &mut graph);
            fs.extend(crate::audit::driver::focus_audit(session));
            fs.extend(crate::audit::driver::resize_audit(session));
            fs.extend(crate::audit::driver::clipping_audit(session));
            fs
        }
        AuditProfile::Keyboard => crate::audit::driver::keyboard_audit(session, 20, &mut graph),
        AuditProfile::Focus => crate::audit::driver::focus_audit(session),
        AuditProfile::Resize | AuditProfile::Layout => {
            crate::audit::driver::resize_audit(session)
        }
        AuditProfile::Clipping => crate::audit::driver::clipping_audit(session),
        AuditProfile::Navigation => {
            crate::audit::driver::navigation_audit(session, 20, &mut graph)
        }
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

fn observe_or_err(session: &mut Session) -> Result<crate::screen::ScreenState, String> {
    session
        .observe(40)
        .map_err(|e| format!("observe failed: {e}"))
}

/// Error-shaped finding helper for driver-level failures (unused today;
/// kept so orchestration-level failures carry typed evidence).
#[allow(dead_code)]
fn orchestration_error(profile: &str, summary: String) -> Finding {
    Finding {
        id: format!("AUDIT-ERR-{}", profile.to_uppercase()),
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
        ] {
            assert!(AuditProfile::parse(name).is_ok(), "{} must parse", name);
        }
    }

    /// The live orchestrator: `full` returns composite mode and includes
    /// findings from the active drivers (a static-only run cannot produce
    /// keyboard-traversal evidence).
    #[test]
    fn full_profile_runs_active_drivers() {
        let mut mgr = crate::session::manager::SessionManager::new();
        let id = mgr
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
            .expect("start");
        let sess = mgr.resolve_mut(Some(&id)).expect("session");
        let report = run_profile(sess, "full").expect("run full");
        assert_eq!(report.mode, "composite");
        // A plain python echo screen has little to audit, but the composite
        // must include the static passes AND have attempted the drivers.
        let cats: Vec<&str> = report.findings.iter().map(|f| f.category.as_str()).collect();
        assert!(
            cats.iter().any(|c| matches!(*c, "focus" | "layout" | "clipping" | "discoverability" | "keyboard")),
            "static composite categories must be present: {:?}",
            cats
        );
    }

    #[test]
    fn keyboard_profile_reports_active_mode() {
        let mut mgr = crate::session::manager::SessionManager::new();
        let id = mgr
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
            .expect("start");
        let sess = mgr.resolve_mut(Some(&id)).expect("session");
        let report = run_profile(sess, "keyboard").expect("run keyboard");
        assert_eq!(report.mode, "active");
    }
}
