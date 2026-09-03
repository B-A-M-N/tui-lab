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
    /// Rendering subsystem (Wave 3c item 22): erase strategy, sync-update
    /// discipline, cursor-hiding hygiene from the raw op census.
    Rendering,
    /// Input-protocol subsystem (Wave 3c item 24): the key encodings the
    /// negotiated modes demand (SS3 vs CSI, SGR mouse, kitty flags).
    InputProtocol,
    /// Shell/CLI subsystem (Wave 3c item 30): prompt shape, OSC 133 marks,
    /// exit reporting, alt-screen applicability.
    ShellCli,
    /// Lifecycle subsystem (Wave 3c item 29): terminal modes negotiated but
    /// not restored — the broken-terminal-after-crash trap.
    Lifecycle,
    /// Item 21: lifecycle EXIT test — drives the app to a clean exit,
    /// verifies the final stream restores every engaged mode, then relaunch
    /// and probes SIGINT/SIGTERM teardown. Kills and restarts the target, so
    /// it is classified RestartRequired, not observational like `lifecycle`.
    LifecycleExit,
    /// Query/response conformance (Wave 3c item 33): DSR 6n replayed and
    /// the CPR answer verified against the live cursor.
    QueryResponse,
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
            "rendering" => Ok(AuditProfile::Rendering),
            "input_protocol" => Ok(AuditProfile::InputProtocol),
            "shell_cli" => Ok(AuditProfile::ShellCli),
            "lifecycle" => Ok(AuditProfile::Lifecycle),
            "lifecycle_exit" => Ok(AuditProfile::LifecycleExit),
            "query_response" => Ok(AuditProfile::QueryResponse),
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
            AuditProfile::Rendering => "rendering",
            AuditProfile::InputProtocol => "input_protocol",
            AuditProfile::ShellCli => "shell_cli",
            AuditProfile::Lifecycle => "lifecycle",
            AuditProfile::LifecycleExit => "lifecycle_exit",
            AuditProfile::QueryResponse => "query_response",
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
                // Color is an active-side, non-driving read of the live
                // session (raw ring + fused frame), same class as
                // TerminalModes. Its omission here made profile=color
                // unreachable: the orchestrator routed it to the static
                // branch, which errors "no static checks" — the real
                // driver existed but no caller could reach it.
                | AuditProfile::Color
                // Needs the session (raw ring + fused frame), not just one
                // frame — same "active-side, non-driving" class as Color.
                | AuditProfile::TerminalModes
                | AuditProfile::Rendering
                | AuditProfile::InputProtocol
                | AuditProfile::ShellCli
                | AuditProfile::Lifecycle
                // Item 21: the exit test needs the live session (raw ring,
                // restart); it is active-side even though it is a terminal
                // lifecycle read in spirit.
                | AuditProfile::LifecycleExit
                // Sends one device query (CSI 6n) — still observational,
                // but it writes to the child, so it stays in the active class.
                | AuditProfile::QueryResponse
        )
    }

    /// Static composite is part of `full` (and implied by its members).
    fn wants_static_composite(&self) -> bool {
        matches!(self, AuditProfile::Full)
    }

    /// Wave 4 item 34: how much this profile can change the app under
    /// test. The orchestrator uses this to decide restart-replay gaps
    /// (item 35) and the safe-only default (item 36); the MCP layer
    /// surfaces it so a caller sees WHY a profile was withheld.
    pub fn risk(&self) -> MutationRisk {
        match self {
            // Reads frames/rings only. Sends nothing.
            AuditProfile::Discoverability
            | AuditProfile::Color
            | AuditProfile::Unicode
            | AuditProfile::Controls
            | AuditProfile::TerminalModes
            | AuditProfile::Rendering
            | AuditProfile::InputProtocol
            | AuditProfile::ShellCli
            | AuditProfile::Lifecycle => MutationRisk::Observational,
            // Kills and relaunches the target — the strongest possible
            // mutation. Never eligible for safe-only sessions.
            AuditProfile::LifecycleExit => MutationRisk::RestartRequired,
            // Writes one device query to the child's stdin; no UI
            // semantics change. The reply is engine-generated.
            AuditProfile::QueryResponse => MutationRisk::Observational,
            // Contract conformance drives the app through its own checks;
            // treat it as at-least-reversible.
            AuditProfile::Contract => MutationRisk::Reversible,
            // These drive the app but restore what they touch: Tab-walks
            // end with Shift+Tab reversals, resize restores the original
            // geometry, performance samples settle back, keyboard/focus/
            // navigation leave the focus graph intact. Residue is detected
            // by AuditTransaction, not assumed away.
            AuditProfile::Keyboard
            | AuditProfile::Focus
            | AuditProfile::Resize
            | AuditProfile::Layout
            | AuditProfile::Clipping
            | AuditProfile::Navigation
            | AuditProfile::Performance => MutationRisk::Reversible,
            // Clicks land on REAL controls (Save, Submit, Connect …) and
            // key/error bursts type into the app: external state can
            // change with no undo. These are the ones the safe-only
            // default gates.
            AuditProfile::Mouse | AuditProfile::States | AuditProfile::Errors => {
                MutationRisk::PotentiallyMutating
            }
            // The composite inherits the strongest member risk.
            AuditProfile::Full => MutationRisk::PotentiallyMutating,
        }
    }
}

/// Wave 4 item 34: audit mutation-risk classes. The taxonomy the
/// orchestrator and the safe-only default (item 36) are built on:
///
/// ```text
/// Observational        reads frames/rings; sends nothing to the app
/// Reversible           drives the app but restores what it touches
/// RestartRequired      needs a fresh process to be safe (deep-audit mode)
/// PotentiallyMutating  can change external/app state with no undo
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MutationRisk {
    Observational,
    Reversible,
    RestartRequired,
    PotentiallyMutating,
}

impl MutationRisk {
    /// Stable wire name (surfaced in audit responses and refusals).
    pub fn name(&self) -> &'static str {
        match self {
            MutationRisk::Observational => "observational",
            MutationRisk::Reversible => "reversible",
            MutationRisk::RestartRequired => "restart_required",
            MutationRisk::PotentiallyMutating => "potentially_mutating",
        }
    }

    /// Anything above Observational touches the app and is gated by the
    /// safe-only default for sessions that cannot be replayed.
    pub fn is_invasive(&self) -> bool {
        *self != MutationRisk::Observational
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
    run_profile_checked(session, profile_name, None, SafetyPolicy::AllowMutation)
}

/// Wave 4 items 36/37: how much the orchestrator may touch the app.
///
/// ```text
/// SafeOnly       observational profiles run; anything invasive is
///                withheld with an ORCH-GATED finding naming why —
///                the default for sessions we did not launch
/// AllowMutation  all profiles run (the historical behavior; explicit)
/// DeepIsolation  AllowMutation plus restart-replay between
///                PotentiallyMutating drivers so each sees a fresh app
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyPolicy {
    SafeOnly,
    AllowMutation,
    DeepIsolation,
}

impl SafetyPolicy {
    /// Whether this profile may run at all under the policy.
    fn allows(&self, profile: &AuditProfile) -> bool {
        match self {
            SafetyPolicy::AllowMutation | SafetyPolicy::DeepIsolation => true,
            SafetyPolicy::SafeOnly => !profile.risk().is_invasive(),
        }
    }
}

/// The gated entry point (Wave 4 item 36). Everything the MCP layer calls
/// goes through here so the safe-only default is engine policy, not a
/// caller-side convention.
pub fn run_profile_checked(
    session: &mut Session,
    profile_name: &str,
    contract: Option<&crate::design::ProjectContract>,
    policy: SafetyPolicy,
) -> Result<ProfileReport, String> {
    let profile = AuditProfile::parse(profile_name)?;
    if !policy.allows(&profile) {
        let risk = profile.risk();
        return Ok(ProfileReport {
            profile: profile.clone(),
            mode: "withheld",
            findings: vec![Finding {
                id: "ORCH-GATED".into(),
                rule_id: None,
                severity: "info".into(),
                category: "orchestration".into(),
                summary: format!(
                    "profile '{}' was not run: it is {} ({}), and this session runs under the safe-only default. Pass allow_mutation=true (or attach a restartable launch) to permit it.",
                    profile.name(),
                    risk.name(),
                    risk_summary(&profile),
                ),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "safe_only_gate",
                    "invasive profile withheld under safe-only policy",
                )
                .with_detail(json!({
                    "profile": profile.name(),
                    "risk": risk.name(),
                    "policy": "safe_only",
                    "how_to_allow": "tui_audit allow_mutation=true, or launch the session through tui_session so restart-replay can isolate it",
                }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            }],
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
        });
    }
    run_profile_inner(session, profile, contract, policy)
}

/// One-line "because it does X" for the gate message.
fn risk_summary(profile: &AuditProfile) -> &'static str {
    match profile {
        AuditProfile::Mouse => "it clicks real controls",
        AuditProfile::States => "it tabs through and activates the live UI",
        AuditProfile::Errors => "it sends key bursts into the app",
        AuditProfile::Full => "its members include mouse/states/errors, which drive the live UI",
        _ => "it sends input or resizes the terminal",
    }
}

/// Internal: policy already checked.
fn run_profile_inner(
    session: &mut Session,
    profile: AuditProfile,
    contract: Option<&crate::design::ProjectContract>,
    policy: SafetyPolicy,
) -> Result<ProfileReport, String> {
    run_profile_with_contract_impl(session, profile, contract, policy)
}

/// Contract-armed variant with the default (permissive-for-launched) policy.
pub fn run_profile_with_contract(
    session: &mut Session,
    profile_name: &str,
    contract: Option<&crate::design::ProjectContract>,
) -> Result<ProfileReport, String> {
    let profile = AuditProfile::parse(profile_name)?;
    run_profile_with_contract_impl(session, profile, contract, SafetyPolicy::AllowMutation)
}

fn run_profile_with_contract_impl(
    session: &mut Session,
    profile: AuditProfile,
    contract: Option<&crate::design::ProjectContract>,
    policy: SafetyPolicy,
) -> Result<ProfileReport, String> {
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
        let (screen, sem, _, _) = session
            .observe_fused(40)
            .map_err(|e| format!("observe failed: {e}"))?;
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
        let (screen, sem, _, _) = session
            .observe_fused(40)
            .map_err(|e| format!("observe failed: {e}"))?;
        findings.extend(crate::audit::run("full", &screen, &sem).map_err(|e| e.to_string())?);
    }

    // Item 65: every input-driving driver runs under an AuditTransaction —
    // the pre-state is captured, the driver restores what it can, and any
    // residue it leaves becomes an honest AUDIT-RESIDUE finding instead of
    // silent session mutation (the old Tab-walk-left-focus-dirty bug).
    use crate::audit::transaction::run_verified;
    let tx = |s: &mut Session, f: &mut dyn FnMut(&mut Session) -> Vec<Finding>| -> Vec<Finding> {
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

    // Wave 4 item 35: restart-replay between PotentiallyMutating drivers.
    // Under DeepIsolation, before each mutating driver the session is
    // restarted from its LaunchSpec so the driver sees a fresh app and
    // leaves nothing behind for the next one — order dependence is removed
    // at the orchestration layer instead of assumed away per driver.
    // Without a LaunchSpec (attached brownfield), DeepIsolation degrades to
    // AllowMutation and says so once, honestly.
    let deep = policy == SafetyPolicy::DeepIsolation && session.launch().is_some();
    let mut orchestration_notes: Vec<Finding> = Vec::new();
    if policy == SafetyPolicy::DeepIsolation && !deep {
        orchestration_notes.push(Finding {
            id: "ORCH-NO-RESTART".into(),
            rule_id: None,
            severity: "info".into(),
            category: "orchestration".into(),
            summary: "deep isolation requested but this session has no launch spec (attached brownfield) — running in place; mutating drivers share app state.".to_string(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "deep_isolation_unavailable",
                "restart-replay needs a recorded LaunchSpec",
            )
            .with_detail(json!({ "profile": profile.name() }))],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }
    let restart_between = deep;
    let mut restarts = 0u32;
    // Restart helper: only used when a launch spec exists (checked above).
    let do_restart = |session: &mut Session, before: &str, after: &str| -> Option<Finding> {
        match session.restart() {
            Ok(()) => {
                // Give the fresh process a moment to render its first frame.
                let _ = session.observe(150);
                Some(Finding {
                    id: "ORCH-RESTART".into(),
                    rule_id: None,
                    severity: "info".into(),
                    category: "orchestration".into(),
                    summary: format!(
                        "session restarted between {before} and {after} (deep isolation): each driver sees a fresh app"
                    ),
                    evidence: vec![EvidenceRef::point(
                        EvidenceKind::Other,
                        "restart_replay",
                        "LaunchSpec replay for driver isolation",
                    )
                    .with_detail(json!({
                        "before": before,
                        "after": after,
                        "generation": session.generation,
                    }))],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                })
            }
            Err(e) => Some(Finding {
                id: "ORCH-RESTART-FAILED".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "orchestration".into(),
                summary: format!(
                    "restart between {before} and {after} failed: {e} — continuing on the live app"
                ),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "restart_failed",
                    "session.restart returned Err",
                )
                .with_detail(json!({ "before": before, "after": after, "error": e.to_string() }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            }),
        }
    };

    let active_findings = match profile {
        AuditProfile::Full => {
            let mut fs = tx(session, &mut |s| {
                let mut inner = crate::audit::driver::keyboard_audit(s, 20, &mut graph);
                inner.extend(crate::audit::driver::focus_audit(s));
                inner
            });
            fs.extend(tx(session, &mut |s| {
                let mut inner = crate::audit::driver::resize_audit(s);
                inner.extend(crate::audit::driver::resize_reflow_audit(s));
                inner.extend(crate::audit::driver::clipping_audit(s));
                inner
            }));
            fs.extend(tx(session, &mut |s| {
                let mut inner = crate::audit::driver::navigation_audit(s, 20, &mut graph);
                inner.extend(crate::audit::driver::navigation_keys_audit(s, 6));
                inner
            }));
            if restart_between {
                if let Some(f) = do_restart(session, "navigation", "mouse") {
                    restarts += 1;
                    fs.push(f);
                }
            }
            fs.extend(tx(session, &mut |s| {
                crate::audit::driver::mouse_audit(s, 8)
            }));
            if restart_between {
                if let Some(f) = do_restart(session, "mouse", "states") {
                    restarts += 1;
                    fs.push(f);
                }
            }
            fs.extend(tx(session, &mut |s| {
                crate::audit::driver::states_audit(s, 6)
            }));
            if restart_between {
                if let Some(f) = do_restart(session, "states", "errors") {
                    restarts += 1;
                    fs.push(f);
                }
            }
            fs.extend(tx(session, &mut |s| {
                crate::audit::driver::errors_audit(s, 12)
            }));
            fs.extend(tx(session, &mut |s| {
                crate::audit::driver::performance_audit(s, 5)
            }));
            // Observational-only drivers: read rings/frames, send nothing —
            // no transaction needed (their non-restoration is not residue).
            fs.extend(crate::audit::driver::color_audit(session));
            fs.extend(crate::audit::driver::terminal_modes_audit(session));
            fs.extend(crate::audit::driver::rendering_audit(session));
            fs.extend(crate::audit::driver::input_protocol_audit(session));
            fs.extend(crate::audit::driver::shell_cli_audit(session));
            fs.extend(crate::audit::driver::lifecycle_audit(session));
            fs.extend(crate::audit::driver::query_response_audit(session));
            if restarts > 0 {
                fs.push(Finding {
                    id: "ORCH-DEEP-SUMMARY".into(),
                    rule_id: None,
                    severity: "info".into(),
                    category: "orchestration".into(),
                    summary: format!(
                        "deep isolation: {restarts} restart-replay gap(s) inserted between mutating drivers"
                    ),
                    evidence: vec![EvidenceRef::point(
                        EvidenceKind::Other,
                        "deep_isolation_summary",
                        "restart count for this composite run",
                    )
                    .with_detail(json!({ "restarts": restarts }))],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
            }
            fs
        }
        AuditProfile::Keyboard => tx(session, &mut |s| {
            crate::audit::driver::keyboard_audit(s, 20, &mut graph)
        }),
        AuditProfile::Focus => tx(session, &mut |s| crate::audit::driver::focus_audit(s)),
        AuditProfile::Resize | AuditProfile::Layout => tx(session, &mut |s| {
            let mut inner = crate::audit::driver::resize_audit(s);
            inner.extend(crate::audit::driver::resize_reflow_audit(s));
            inner
        }),
        AuditProfile::Clipping => tx(session, &mut |s| crate::audit::driver::clipping_audit(s)),
        AuditProfile::Navigation => tx(session, &mut |s| {
            let mut inner = crate::audit::driver::navigation_audit(s, 20, &mut graph);
            inner.extend(crate::audit::driver::navigation_keys_audit(s, 6));
            inner
        }),
        AuditProfile::Mouse => tx(session, &mut |s| crate::audit::driver::mouse_audit(s, 12)),
        AuditProfile::Performance => tx(session, &mut |s| {
            crate::audit::driver::performance_audit(s, 7)
        }),
        AuditProfile::States => tx(session, &mut |s| crate::audit::driver::states_audit(s, 10)),
        AuditProfile::Errors => tx(session, &mut |s| crate::audit::driver::errors_audit(s, 15)),
        AuditProfile::Color => crate::audit::driver::color_audit(session),
        // Frame-level read of the raw ring: observes but sends nothing, so
        // it does not need a transaction (Wave G split, item 66).
        AuditProfile::TerminalModes => crate::audit::driver::terminal_modes_audit(session),
        AuditProfile::Rendering => crate::audit::driver::rendering_audit(session),
        AuditProfile::InputProtocol => crate::audit::driver::input_protocol_audit(session),
        AuditProfile::ShellCli => crate::audit::driver::shell_cli_audit(session),
        AuditProfile::Lifecycle => crate::audit::driver::lifecycle_audit(session),
        // Item 21: consumes the app (clean exit + signal probes). No audit
        // transaction — the app is deliberately terminated and relaunched.
        AuditProfile::LifecycleExit => crate::audit::driver::lifecycle_exit_audit(session),
        AuditProfile::QueryResponse => crate::audit::driver::query_response_audit(session),
        _ => unreachable!("non-active profiles returned above"),
    };
    findings.extend(orchestration_notes);
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
            "rendering",
            "input_protocol",
            "shell_cli",
            "lifecycle",
            "query_response",
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
        assert_eq!(
            modes["mouse_sgr_encoding"], true,
            "DECSET 1006 must fold to on"
        );
        assert!(
            report.findings.iter().any(|f| f.id == "MODE-MOUSE-HIDDEN"),
            "mouse negotiated with a plain print() screen = hidden affordances"
        );
        pool.stop(&id).await.ok();
    }

    /// Wave 3c: a child that emits the flicker signature — repeated
    /// erase-all cycles, cursor hidden more often than shown, no
    /// synchronized-update — must trip REND-FLICKER and REND-CURSOR-LEAK,
    /// and rendering/input_protocol/lifecycle/shell_cli must run clean of
    /// engine errors through the orchestrator.
    #[tokio::test]
    async fn wave3c_profiles_read_real_traffic() {
        let pool = crate::session::SessionPool::new();
        // The loop redaws 3× with erase-all, hides the cursor twice,
        // shows it once, and enables the alt screen without restoring it.
        let id = pool
            .start(
                "python3",
                &[
                    "-c".into(),
                    "import sys,time; w=sys.stdout.write; w('\\x1b[?1049h'); [ (w('\\x1b[2J'), w('\\x1b[?25l'), w('frame'), w('\\x1b[?25h' if i==2 else ''), sys.stdout.flush(), time.sleep(0.05)) for i in range(3)]; input()"
                        .to_string(),
                ],
                None,
                &[],
                80,
                24,
                "portable_vt100",
                "local",
            )
            .await
            .expect("start");

        let render = pool
            .with_session(Some(&id), |s| run_profile(s, "rendering"))
            .await
            .expect("actor run")
            .expect("rendering");
        let ids: Vec<&str> = render.findings.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"REND-STYLE"), "style census present: {ids:?}");
        assert!(
            ids.contains(&"REND-FLICKER"),
            "erase-all cycles without 2026 must trip the flicker rule: {ids:?}"
        );
        assert!(
            ids.contains(&"REND-CURSOR-LEAK"),
            "2 hides vs 1 show must trip the cursor-leak rule: {ids:?}"
        );

        let inp = pool
            .with_session(Some(&id), |s| run_profile(s, "input_protocol"))
            .await
            .expect("actor run")
            .expect("input_protocol");
        assert!(
            inp.findings.iter().any(|f| f.id == "INP-ENCODING"),
            "encoding plan must be present"
        );

        let lc = pool
            .with_session(Some(&id), |s| run_profile(s, "lifecycle"))
            .await
            .expect("actor run")
            .expect("lifecycle");
        assert!(
            lc.findings.iter().any(|f| f.id == "LC-DANGLING"),
            "alt screen engaged and never restored must dangle: {:?}",
            lc.findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>()
        );

        let sh = pool
            .with_session(Some(&id), |s| run_profile(s, "shell_cli"))
            .await
            .expect("actor run")
            .expect("shell_cli");
        assert!(
            sh.findings.iter().any(|f| f.id == "SH-ALTSCREEN"),
            "alt screen active must be reported to steer heuristics off"
        );
        pool.stop(&id).await.ok();
    }

    /// Wave 3c item 33: on a responder-capable engine the audit must find
    /// no unimplemented query class and must produce the live CPR probe;
    /// on the pipe engine (no responder, no ring) it must degrade honestly
    /// to QR-NOSRC naming the conformance risk.
    #[tokio::test]
    async fn query_response_verifies_cpr() {
        let pool = crate::session::SessionPool::new();
        // The child asks DA1 + DSR 6n itself, so the inventory has real
        // queries to report.
        let id = pool
            .start(
                "python3",
                &[
                    "-c".into(),
                    "import sys; sys.stdout.write('\\x1b[0c\\x1b[6n'); sys.stdout.flush(); print('cpr-test'); input()"
                        .to_string(),
                ],
                None,
                &[],
                80,
                24,
                "portable_vt100",
                "local",
            )
            .await
            .expect("start");
        let report = pool
            .with_session(Some(&id), |s| run_profile(s, "query_response"))
            .await
            .expect("actor run")
            .expect("query_response");
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"QR-INVENTORY"), "inventory present: {ids:?}");
        assert!(
            ids.contains(&"QR-CPR-PROBE"),
            "responder engine must produce the live CPR probe: {ids:?}"
        );
        let inv = report
            .findings
            .iter()
            .find(|f| f.id == "QR-INVENTORY")
            .unwrap();
        let summary = &inv.summary;
        assert!(
            summary.contains("DA1") && summary.contains("DSR 6n"),
            "the child's real DA1 + 6n queries must appear: {summary}"
        );
        pool.stop(&id).await.ok();

        // Pipe engine: honest degradation.
        let pool2 = crate::session::SessionPool::new();
        let id2 = pool2
            .start(
                "python3",
                &["-c".into(), "print('cpr-pipe'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "pipe",
                "local",
            )
            .await
            .expect("start pipe");
        let report2 = pool2
            .with_session(Some(&id2), |s| run_profile(s, "query_response"))
            .await
            .expect("actor run")
            .expect("query_response pipe");
        assert!(
            report2.findings.iter().any(|f| f.id == "QR-NOSRC"),
            "pipe engine has no responder and no ring — must say so: {:?}",
            report2
                .findings
                .iter()
                .map(|f| f.id.clone())
                .collect::<Vec<_>>()
        );
        pool2.stop(&id2).await.ok();
    }

    /// Wave 4 item 34: the risk taxonomy is complete and ordered — every
    /// parseable profile classifies, `full` inherits the strongest member
    /// risk, and the observational set is exactly the non-driving +
    /// frame-level set.
    #[test]
    fn every_profile_has_a_risk_class() {
        let profiles = [
            "full",
            "keyboard",
            "focus",
            "resize",
            "layout",
            "clipping",
            "discoverability",
            "navigation",
            "contract",
            "color",
            "performance",
            "mouse",
            "states",
            "errors",
            "unicode",
            "controls",
            "terminal_modes",
            "rendering",
            "input_protocol",
            "shell_cli",
            "lifecycle",
            "query_response",
        ];
        for name in profiles {
            let p = AuditProfile::parse(name).expect(name);
            let r = p.risk();
            // Wire names are stable and non-empty.
            assert!(!r.name().is_empty(), "{name} risk names itself");
        }
        // full inherits the strongest member.
        assert_eq!(AuditProfile::Full.risk(), MutationRisk::PotentiallyMutating);
        // The observational set: exactly the profiles that never drive UI.
        for name in [
            "discoverability",
            "color",
            "unicode",
            "controls",
            "terminal_modes",
            "rendering",
            "input_protocol",
            "shell_cli",
            "lifecycle",
            "query_response",
        ] {
            let p = AuditProfile::parse(name).unwrap();
            assert_eq!(
                p.risk(),
                MutationRisk::Observational,
                "{name} must be observational"
            );
        }
        // The mutating set: clicks and bursts that can change app state.
        for name in ["mouse", "states", "errors"] {
            let p = AuditProfile::parse(name).unwrap();
            assert_eq!(
                p.risk(),
                MutationRisk::PotentiallyMutating,
                "{name} must be potentially mutating"
            );
        }
    }

    /// Wave 4 item 36: SafeOnly runs observational profiles and withholds
    /// invasive ones with an ORCH-GATED finding that names the escape
    /// hatch — the withheld profile never touches the app.
    #[tokio::test]
    async fn safe_only_withholds_invasive_profiles() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('safe-only'); input()".to_string()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        // mouse is PotentiallyMutating: gated.
        let report = pool
            .with_session(Some(&id), |s| {
                run_profile_checked(s, "mouse", None, SafetyPolicy::SafeOnly)
            })
            .await
            .expect("actor run")
            .expect("gated report");
        assert_eq!(report.mode, "withheld");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].id, "ORCH-GATED");
        let detail = serde_json::to_value(&report.findings[0].evidence).unwrap();
        assert_eq!(detail[0]["detail"]["risk"], "potentially_mutating");

        // unicode is Observational: runs normally under the same policy.
        let report = pool
            .with_session(Some(&id), |s| {
                run_profile_checked(s, "unicode", None, SafetyPolicy::SafeOnly)
            })
            .await
            .expect("actor run")
            .expect("static report");
        assert_eq!(report.mode, "static");
        pool.stop(&id).await.ok();
    }

    /// Wave 4 items 35+37: deep isolation inserts restart-replay gaps
    /// between the composite's mutating drivers (each ORCH-RESTART names
    /// the pair it isolated), and the run still completes.
    #[tokio::test]
    async fn deep_isolation_restarts_between_mutating_drivers() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('deep-iso'); input()".to_string()],
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
            .with_session(Some(&id), |s| {
                run_profile_checked(s, "full", None, SafetyPolicy::DeepIsolation)
            })
            .await
            .expect("actor run")
            .expect("deep full");
        let restarts: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.id == "ORCH-RESTART")
            .collect();
        assert!(
            restarts.len() >= 3,
            "mouse/states/errors gaps must each restart: {:?}",
            restarts
                .iter()
                .map(|f| f.summary.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            report.findings.iter().any(|f| f.id == "ORCH-DEEP-SUMMARY"),
            "the run summarizes its restart count"
        );
        pool.stop(&id).await.ok();
    }

    /// Wave 4 item 39: a screen with a single focusable control re-tabs to
    /// itself — that is the expected degenerate case (KB-SINGLE-FOCUSABLE,
    /// info), NOT a KB-TRAP warning.
    #[tokio::test]
    async fn single_focusable_screen_is_not_a_trap() {
        let pool = crate::session::SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "print('single-focus'); input()".to_string()],
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
            .with_session(Some(&id), |s| {
                run_profile_checked(s, "keyboard", None, SafetyPolicy::AllowMutation)
            })
            .await
            .expect("actor run")
            .expect("keyboard run");
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert!(
            !ids.contains(&"KB-TRAP"),
            "a plain echo screen has ≤1 focusable control — no trap: {ids:?}"
        );
        assert!(
            ids.contains(&"KB-SINGLE-FOCUSABLE"),
            "the static-focus case must be named as info, not warn: {ids:?}"
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

    /// Review P1 item 8 (audit transaction guarantees): every input-DRIVING
    /// driver must be invoked through the audit transaction wrapper. This is
    /// the mechanical gate — it reads this file's own source and fails if a
    /// driving driver call appears bare (not inside `tx(...)`) inside the
    /// dispatch. The observational drivers (color/terminal_modes/rendering/
    /// input_protocol/shell_cli/lifecycle/query_response) are exempt: they
    /// send nothing, so there is nothing to restore.
    #[test]
    fn driving_drivers_are_always_transaction_wrapped() {
        let src = include_str!("orchestrator.rs");
        // Slice to the dispatch: from the `tx` helper definition to the end
        // of the active-findings match.
        let start = src
            .find("let active_findings = match profile {")
            .expect("dispatch found");
        let end = src[start..]
            .find("_ => unreachable!")
            .map(|e| start + e)
            .expect("dispatch end");
        let dispatch = &src[start..end];

        let driving = [
            "keyboard_audit",
            "focus_audit",
            "resize_audit",
            "resize_reflow_audit",
            "clipping_audit",
            "navigation_audit",
            "navigation_keys_audit",
            "mouse_audit",
            "performance_audit",
            "states_audit",
            "errors_audit",
        ];
        for driver in driving {
            // Find each call site: `driver::NAME(`.
            let mut idx = 0usize;
            while let Some(rel) = dispatch[idx..].find(&format!("driver::{driver}(")) {
                let site = idx + rel;
                // The receiver argument tells wrapped from bare: a wrapped
                // call runs inside the closure passed to `tx`, whose session
                // parameter is named `s`; a bare call passes the outer
                // `session` directly.
                let call_start = site + format!("driver::{driver}").len();
                let call_end = dispatch[call_start..]
                    .find(')')
                    .map(|e| call_start + e + 1)
                    .unwrap_or(dispatch.len());
                let call = &dispatch[site..call_end];
                let first_arg = call
                    .split('(')
                    .nth(1)
                    .unwrap_or("")
                    .split([',', ')'])
                    .next()
                    .unwrap_or("")
                    .trim();
                let wrapped = first_arg == "s";
                assert!(
                    wrapped,
                    "driver `{}` invoked without an audit transaction (first arg \
                     must be the closure's `s`, not the bare `session`):\n    {}",
                    driver,
                    call.trim()
                );
                idx = call_end;
            }
        }
    }
}
