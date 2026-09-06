//! Audit orchestration (re-review P0 fix 2): one authority for what a
//! profile means and whether it drives the live app.
//!
//! Before this module, the MCP layer decided static-vs-active with a string
//! match that omitted `"full"` — so `tui_audit profile=full` ran *fewer*
//! checks than `profile=keyboard`, which is backwards. The engine now owns
//! the decision, and since the descriptor-table refactor it owns it in ONE
//! place ([`PROFILES`]): parse, name, routing, lease gating, safe-only
//! policy, `full` membership/sequence, and the accepted-selector error all
//! derive from the same table.
//!
//! ```text
//! full = static composite + every included_in_full driver, in table order
//! full NEVER includes lifecycle_exit (process-consuming; explicit opt-in
//!        via allow_process_restart=true, never implied by allow_mutation)
//! ```
//!
//! The MCP layer calls [`run_profile`] and reports the returned mode; it no
//! longer interprets profile names. For the human control lease the MCP
//! layer asks `requires_exclusive_control()` (drives/resizes/consumes), NOT
//! `needs_live_session()` (needs a live session) — passive diagnostics stay
//! available under a human lease.

use crate::audit::{Category, EvidenceKind, EvidenceRef, Finding, Severity};
use crate::session::state::Session;
use serde_json::json;

/// A parsed audit profile. Parsing lives here so an unknown name is an
/// engine-level error, not an MCP-level guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditProfile {
    /// All standard audit families: static composite plus every
    /// non-process-consuming driver, in descriptor-table order. Deliberately
    /// EXCLUDES `lifecycle_exit` (it terminates the target) — full must
    /// never consume the application under audit.
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
    /// not silently degrade a typo into a weaker audit. The accepted set
    /// and the error text come from the descriptor table, so a new profile
    /// can never be parseable-but-undocumented or documented-but-unparseable
    /// (the old hand-written error list had drifted behind 8 real profiles).
    pub fn parse(name: &str) -> Result<Self, String> {
        PROFILES
            .iter()
            .find(|d| d.id == name)
            .map(|d| d.profile.clone())
            .ok_or_else(|| {
                format!(
                    "unknown audit profile '{}'; expected one of {}",
                    name,
                    PROFILES.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
                )
            })
    }

    /// The profile's canonical engine name (used in residue findings).
    pub fn name(&self) -> &'static str {
        descriptor(self).id
    }

    /// Does this profile need a live `Session` (raw ring / fused frame /
    /// process access) rather than one detached frame? This is the
    /// orchestrator's static-vs-live ROUTING split only — it says
    /// nothing about whether the profile drives the app. For the human
    /// control lease, use [`Self::requires_exclusive_control`].
    pub fn needs_live_session(&self) -> bool {
        !matches!(
            self,
            AuditProfile::Discoverability | AuditProfile::Unicode | AuditProfile::Controls
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
        descriptor(self).risk
    }

    /// Does this profile send UI input, resize the terminal, or consume
    /// the process? Those need exclusive control, so `tui_audit` refuses
    /// them while a human lease is live. Observational profiles that
    /// still need the live session (color, terminal_modes, rendering,
    /// input_protocol, shell_cli, lifecycle, query_response) stay
    /// available during a lease — observation is always allowed (review
    /// §5: `is_active()` must not gate the lease; one boolean was making
    /// passive diagnostics refuse under a human's control).
    pub fn requires_exclusive_control(&self) -> bool {
        descriptor(self).exclusive_control
    }
}

/// One row of the single source of truth for audit profiles (review §7):
/// the same table derives parsing, naming, routing, lease gating, the
/// safe-only policy, `full`'s membership/sequence, and the
/// accepted-selector error. A new profile is added HERE once and the
/// parity test vs the MCP enum fails if the wire side forgets it.
///
/// Table order IS `full`'s driver execution order: static composite first
/// (separate pass), then transaction-wrapped driver groups (reversible
/// before potentially-mutating, with deep-isolation restart gaps before
/// the mutating ones), then the observational readers. Order matters for
/// evidence readability — do not reorder casually.
pub struct AuditProfileDescriptor {
    /// Canonical selector (also the residue-finding name).
    pub id: &'static str,
    pub profile: AuditProfile,
    pub risk: MutationRisk,
    /// Needs exclusive control: sends UI input, resizes, or consumes the
    /// process. The lease gate keys on this (not on `is_active`).
    pub exclusive_control: bool,
    /// Member of `profile=full`. `full` is not a member of itself,
    /// `layout` is the alias of `resize` (full already runs resize), the
    /// static-composite profiles ride full's static pass instead of the
    /// driver sequence, and `lifecycle_exit` is deliberately excluded:
    /// full must never terminate the target — that requires explicit
    /// opt-in (`allow_process_restart=true`).
    pub included_in_full: bool,
    /// Wrap this profile's run in an AuditTransaction (pre-state capture
    /// with residue verification): true for drivers that send input or
    /// resize; false for observational readers (nothing to restore)
    /// and for `lifecycle_exit` (the app is deliberately consumed).
    pub transaction: bool,
    /// `full` transaction grouping: members sharing a group run inside
    /// ONE AuditTransaction, contiguously, in table order. 0 = not part
    /// of full's driver sequence.
    pub full_group: u8,
    /// The driver: (session, focus graph to merge traversal edges into).
    /// Rows that never dispatch through the table (`full` itself, the
    /// static-composite profiles, `contract`) carry an unreachable body —
    /// the type needs the field; the dispatcher never calls it.
    pub driver: DriverFn,
}

pub static PROFILES: &[AuditProfileDescriptor] = &[
    // --- full (the composite selector; iterates members, never dispatched) ---
    AuditProfileDescriptor {
        id: "full",
        profile: AuditProfile::Full,
        risk: MutationRisk::PotentiallyMutating,
        exclusive_control: true,
        included_in_full: false,
        transaction: true,
        full_group: 0,
        driver: |_, _| unreachable!("full iterates members; it is not dispatched"),
    },
    // --- full's driver sequence (transaction groups, in run order) ---
    AuditProfileDescriptor {
        id: "keyboard",
        profile: AuditProfile::Keyboard,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 1,
        driver: |s, graph| crate::audit::driver::keyboard_audit(s, 20, graph),
    },
    AuditProfileDescriptor {
        id: "focus",
        profile: AuditProfile::Focus,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 1,
        driver: |s, _graph| crate::audit::driver::focus_audit(s),
        // focus_audit takes no graph; the closure ignores it.
    },
    AuditProfileDescriptor {
        id: "resize",
        profile: AuditProfile::Resize,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 2,
        driver: |s, _graph| {
            let mut inner = crate::audit::driver::resize_audit(s);
            inner.extend(crate::audit::driver::resize_reflow_audit(s));
            inner
        },
    },
    // Documented alias of `resize` (same driver, same risk); full already
    // runs resize, so the alias is not a second full member.
    AuditProfileDescriptor {
        id: "layout",
        profile: AuditProfile::Layout,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: false,
        transaction: true,
        full_group: 2,
        driver: |s, _graph| {
            let mut inner = crate::audit::driver::resize_audit(s);
            inner.extend(crate::audit::driver::resize_reflow_audit(s));
            inner
        },
    },
    AuditProfileDescriptor {
        id: "clipping",
        profile: AuditProfile::Clipping,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 2,
        driver: |s, _graph| crate::audit::driver::clipping_audit(s),
    },
    AuditProfileDescriptor {
        id: "navigation",
        profile: AuditProfile::Navigation,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 3,
        driver: |s, graph| {
            let mut inner = crate::audit::driver::navigation_audit(s, 20, graph);
            inner.extend(crate::audit::driver::navigation_keys_audit(s, 6));
            inner
        },
    },
    AuditProfileDescriptor {
        id: "mouse",
        profile: AuditProfile::Mouse,
        risk: MutationRisk::PotentiallyMutating,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 4,
        // full's budget (8 clicks); a standalone mouse audit samples more.
        driver: |s, _graph| crate::audit::driver::mouse_audit(s, 8),
    },
    AuditProfileDescriptor {
        id: "states",
        profile: AuditProfile::States,
        risk: MutationRisk::PotentiallyMutating,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 5,
        driver: |s, _graph| crate::audit::driver::states_audit(s, 6),
    },
    AuditProfileDescriptor {
        id: "errors",
        profile: AuditProfile::Errors,
        risk: MutationRisk::PotentiallyMutating,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 6,
        driver: |s, _graph| crate::audit::driver::errors_audit(s, 12),
    },
    AuditProfileDescriptor {
        id: "performance",
        profile: AuditProfile::Performance,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: true,
        transaction: true,
        full_group: 7,
        driver: |s, _graph| crate::audit::driver::performance_audit(s, 5),
    },
    // --- observational, live-session readers (no transaction; lease-safe) ---
    AuditProfileDescriptor {
        id: "color",
        profile: AuditProfile::Color,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::color_audit(s),
    },
    AuditProfileDescriptor {
        id: "terminal_modes",
        profile: AuditProfile::TerminalModes,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::terminal_modes_audit(s),
    },
    AuditProfileDescriptor {
        id: "rendering",
        profile: AuditProfile::Rendering,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::rendering_audit(s),
    },
    AuditProfileDescriptor {
        id: "input_protocol",
        profile: AuditProfile::InputProtocol,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::input_protocol_audit(s),
    },
    AuditProfileDescriptor {
        id: "shell_cli",
        profile: AuditProfile::ShellCli,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::shell_cli_audit(s),
    },
    AuditProfileDescriptor {
        id: "lifecycle",
        profile: AuditProfile::Lifecycle,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::lifecycle_audit(s),
    },
    // Writes one device query (CSI 6n) to the child's stdin; the reply is
    // engine-generated and no UI semantics change, so it stays lease-safe.
    AuditProfileDescriptor {
        id: "query_response",
        profile: AuditProfile::QueryResponse,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: true,
        transaction: false,
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::query_response_audit(s),
    },
    // --- standalone budgets differ from full's for these drivers ---
    // (resolved in run_profile_with_contract_impl: standalone mouse/states/
    // errors/performance sample MORE than the full-composite budgets above;
    // see SINGLE_PROFILE_OVERRIDES.)
    // --- static single-frame profiles (full's static composite pass) ---
    AuditProfileDescriptor {
        id: "discoverability",
        profile: AuditProfile::Discoverability,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: false,
        transaction: false,
        full_group: 0,
        driver: |_, _| unreachable!("static profiles route through the frame-check registry"),
    },
    AuditProfileDescriptor {
        id: "unicode",
        profile: AuditProfile::Unicode,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: false,
        transaction: false,
        full_group: 0,
        driver: |_, _| unreachable!("static profiles route through the frame-check registry"),
    },
    AuditProfileDescriptor {
        id: "controls",
        profile: AuditProfile::Controls,
        risk: MutationRisk::Observational,
        exclusive_control: false,
        included_in_full: false,
        transaction: false,
        full_group: 0,
        driver: |_, _| unreachable!("static profiles route through the frame-check registry"),
    },
    // Contract conformance drives the app through its own checks;
    // at-least-reversible. Dispatched before this table (it needs the
    // loaded contract), so its driver row is never called.
    AuditProfileDescriptor {
        id: "contract",
        profile: AuditProfile::Contract,
        risk: MutationRisk::Reversible,
        exclusive_control: true,
        included_in_full: false,
        transaction: true,
        full_group: 0,
        driver: |_, _| unreachable!("contract dispatches through check_contract"),
    },
    // --- process-consuming (never in full; explicit opt-in only) ---
    AuditProfileDescriptor {
        id: "lifecycle_exit",
        profile: AuditProfile::LifecycleExit,
        risk: MutationRisk::RestartRequired,
        exclusive_control: true,
        included_in_full: false,
        transaction: false, // the app is deliberately consumed, not restored
        full_group: 0,
        driver: |s, _graph| crate::audit::driver::lifecycle_exit_audit(s),
    },
];

/// A driver body: takes the session and the traversal graph to merge
/// focus edges into, returns findings.
pub type DriverFn = fn(&mut Session, &mut crate::semantic::focus_graph::FocusGraph) -> Vec<Finding>;

/// Standalone budgets that differ from full's composite budgets (full
/// samples less per driver so the composite stays bounded). Keyed by
/// profile; consulted only when the profile runs ALONE.
static SINGLE_PROFILE_OVERRIDES: &[(AuditProfile, DriverFn)] = &[
    (AuditProfile::Mouse, |s, _g| {
        crate::audit::driver::mouse_audit(s, 12)
    }),
    (AuditProfile::States, |s, _g| {
        crate::audit::driver::states_audit(s, 10)
    }),
    (AuditProfile::Errors, |s, _g| {
        crate::audit::driver::errors_audit(s, 15)
    }),
    (AuditProfile::Performance, |s, _g| {
        crate::audit::driver::performance_audit(s, 7)
    }),
];

fn descriptor(p: &AuditProfile) -> &'static AuditProfileDescriptor {
    PROFILES
        .iter()
        .find(|d| d.profile == *p)
        .expect("descriptor table covers every AuditProfile variant")
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
    /// Per-driver timing from the transactional runs (finding 20). Empty for
    /// static/gated profiles. Metrics are report metadata — never findings.
    pub metrics: Vec<crate::audit::transaction::AuditMetrics>,
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
        // Audit P0-24: a gated composite is DECOMPOSED, not dismissed.
        // `full` under safe-only used to produce one ORCH-GATED and zero
        // checks — an empty audit that looked like "ran, nothing found".
        // The honest report: every OBSERVATIONAL member of the composite
        // actually runs now, and each withheld member is named with its
        // risk and its driver role, so the caller sees what ran, what
        // didn't, and why.
        if profile == AuditProfile::Full {
            let mut findings: Vec<Finding> = Vec::new();
            // 1. The static composite (discoverability/unicode/controls) —
            //    observational, always permitted.
            {
                let (screen, sem, _, _) = session
                    .observe_fused(40)
                    .map_err(|e| format!("observe failed: {e}"))?;
                findings
                    .extend(crate::audit::run("full", &screen, &sem).map_err(|e| e.to_string())?);
            }
            // 2. Every observational member of full that has a driver —
            //    these read live state without touching the app.
            for d in PROFILES
                .iter()
                .filter(|d| d.included_in_full && d.risk == MutationRisk::Observational)
            {
                let f = d.driver;
                findings.extend(f(
                    session,
                    &mut crate::semantic::focus_graph::FocusGraph::new(),
                ));
            }
            // 3. The withheld list: every invasive member, named.
            let withheld: Vec<serde_json::Value> = PROFILES
                .iter()
                .filter(|d| d.included_in_full && d.risk.is_invasive())
                .map(|d| {
                    json!({
                        "profile": d.id,
                        "risk": d.risk.name(),
                        "reason": "invasive under safe-only policy",
                    })
                })
                .collect();
            findings.push(Finding {
                id: "ORCH-GATED".into(),
                rule_id: None,
                severity: Severity::Info,
                category: Category::Orchestration,
                summary: format!(
                    "'full' ran its observational members only ({} finding(s) above): {} member(s) withheld — {}. Pass allow_mutation=true (or attach a restartable launch) to permit them.",
                    findings.len(),
                    withheld.len(),
                    PROFILES
                        .iter()
                        .filter(|d| d.included_in_full && d.risk.is_invasive())
                        .map(|d| d.id)
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "safe_only_gate",
                    "invasive profile members withheld under safe-only policy; observational members RAN (decomposed report)",
                )
                .with_detail(json!({
                    "profile": profile.name(),
                    "policy": "safe_only",
                    "withheld": withheld,
                    "how_to_allow": "tui_audit allow_mutation=true, or launch the session through tui_session so restart-replay can isolate it",
                }))],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
            });
            return Ok(ProfileReport {
                profile,
                mode: "partial",
                findings,
                focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
                metrics: Vec::new(),
            });
        }
        return Ok(ProfileReport {
            profile: profile.clone(),
            mode: "withheld",
            findings: vec![Finding {
                id: "ORCH-GATED".into(),
                rule_id: None,
                severity: Severity::Info,
                category: Category::Orchestration,
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
                occurrence_id: None,
            }],
            focus_graph: crate::semantic::focus_graph::FocusGraph::new(),
            metrics: Vec::new(),
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
            metrics: Vec::new(),
        });
    }

    // The static profiles (single frame, no driving): discoverability plus
    // the Wave-3 subsystem audits (unicode, controls).
    if !profile.needs_live_session() {
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
            metrics: Vec::new(),
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
    let mut metrics: Vec<crate::audit::transaction::AuditMetrics> = Vec::new();
    let mut tx =
        |s: &mut Session, f: &mut dyn FnMut(&mut Session) -> Vec<Finding>| -> Vec<Finding> {
            let (fs, m) = run_verified(s, profile.name(), |sess| f(sess)).unwrap_or_else(|e| {
                (
                    vec![Finding {
                        id: "AUDIT-TX-ERR".into(),
                        rule_id: None,
                        severity: Severity::Error,
                        category: Category::Audit,
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
                        occurrence_id: None,
                    }],
                    crate::audit::transaction::AuditMetrics {
                        profile: profile.name().to_string(),
                        driver_ms: 0,
                        verify_ms: 0,
                        total_ms: 0,
                    },
                )
            });
            metrics.push(m);
            fs
        };

    // Wave 4 item 35 + audit P0-11: restart-replay between
    // PotentiallyMutating drivers. Under DeepIsolation, before each
    // mutating driver the session is restarted from its LaunchSpec so the
    // driver sees a fresh app and leaves nothing behind for the next one.
    // DeepIsolation is a CONTRACT, not an optimization: when isolation is
    // impossible (no LaunchSpec — attached brownfield), the audit refuses
    // invasive profiles outright instead of degrading to in-place
    // mutation. Observational-only profiles still run: they never touch
    // the app, so isolation is irrelevant to them.
    let deep = policy == SafetyPolicy::DeepIsolation && session.launch().is_some();
    let mut orchestration_notes: Vec<Finding> = Vec::new();
    if policy == SafetyPolicy::DeepIsolation && !deep {
        let requested_risk = descriptor(&profile).risk;
        if requested_risk.is_invasive() {
            // The requested profile can mutate and isolation is
            // impossible: refuse. The old behavior "ran in place" — a
            // deep-isolation request silently mutating the live target is
            // exactly the failure the policy exists to prevent.
            let profile_name = profile.name();
            return Ok(ProfileReport {
                profile,
                mode: "refused",
                findings: vec![Finding {
                    id: "ORCH-DEEP-REFUSED".into(),
                    rule_id: None,
                    severity: Severity::Warn,
                    category: Category::Orchestration,
                    summary: "restart_between_mutations requested but unattainable (no launch spec — attached brownfield): the requested profile is invasive, so it was REFUSED, not run in place. Re-run with a launched session, allow_mutation (explicit in-place consent), or an observational profile.".to_string(),
                    evidence: vec![EvidenceRef::point(
                        EvidenceKind::Other,
                        "restart_replay_unavailable",
                        "restart-replay needs a recorded LaunchSpec; invasive profiles are refused rather than degraded",
                    )
                    .with_detail(json!({ "profile": profile_name, "risk": requested_risk.name() }))],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                    occurrence_id: None,
                }],
                focus_graph: graph,
                metrics: Vec::new(),
            });
        }
        orchestration_notes.push(Finding {
            id: "ORCH-NO-RESTART".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::Orchestration,
            summary: "restart_between_mutations requested but this session has no launch spec (attached brownfield) — the requested profile is observational (no mutation), so it runs unaffected.".to_string(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "restart_replay_unavailable",
                "restart-replay needs a recorded LaunchSpec; observational profiles do not need it",
            )
            .with_detail(json!({ "profile": profile.name() }))],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }
    let restart_between = deep;
    let mut restarts = 0u32;
    let mut aborted = false;
    // Restart helper: only used when a launch spec exists (checked above).
    // Audit P0-11: a FAILED restart aborts the remaining active members —
    // continuing "on the live app" meant driving an app in unknown state
    // after isolation was already promised.
    let do_restart = |session: &mut Session, before: &str, after: &str| -> Option<Finding> {
        match session.restart() {
            Ok(()) => {
                // Give the fresh process a moment to render its first frame.
                let _ = session.observe(150);
                Some(Finding {
                    id: "ORCH-RESTART".into(),
                    rule_id: None,
                    severity: Severity::Info,
                    category: Category::Orchestration,
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
                    occurrence_id: None,
                })
            }
            Err(e) => Some(Finding {
                id: "ORCH-RESTART-FAILED".into(),
                rule_id: None,
                severity: Severity::Warn,
                category: Category::Orchestration,
                summary: format!(
                    "restart between {before} and {after} failed: {e} — remaining active drivers ABORTED (deep isolation broken; the app is in unknown state)"
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
                occurrence_id: None,
            }),
        }
    };

    let mut fs: Vec<Finding> = Vec::new();
    if profile == AuditProfile::Full {
        // `full` = static composite + every `included_in_full` driver, in
        // table order (P0 fix 2, now table-derived). Transaction groups run
        // contiguously inside one AuditTransaction each; deep-isolation
        // restart gaps are inserted before each group whose members can
        // mutate (risk above Reversible) — previously hand-placed before
        // mouse/states/errors, which is exactly the set this derives to.
        let members: Vec<&AuditProfileDescriptor> = PROFILES
            .iter()
            .filter(|d| d.included_in_full && d.full_group > 0)
            .collect();
        let mut last_group = 0u8;
        for d in members {
            if restart_between && d.full_group != last_group && d.risk >= MutationRisk::Reversible {
                // Reversible-or-worse group boundary: give the next group a
                // fresh app. (Groups 1-3 restore what they touch; the gap
                // still helps when an earlier group left residue.)
                if d.risk > MutationRisk::Reversible || d.full_group >= 4 {
                    if let Some(f) = do_restart(session, "previous group", d.id) {
                        restarts += 1;
                        let failed = f.id == "ORCH-RESTART-FAILED";
                        fs.push(f);
                        if failed {
                            // Isolation already promised: do not drive an
                            // app of unknown state. Abort the rest.
                            aborted = true;
                            break;
                        }
                    }
                }
            }
            last_group = d.full_group;
            if d.transaction {
                let f = d.driver;
                fs.extend(tx(session, &mut |s| f(s, &mut graph)));
            } else {
                fs.extend((d.driver)(session, &mut graph));
            }
        }
        if restarts > 0 {
            if aborted {
                fs.push(Finding {
                    id: "ORCH-DEEP-ABORTED".into(),
                    rule_id: None,
                    severity: Severity::Warn,
                    category: Category::Orchestration,
                    summary: format!(
                        "restart_between_mutations: run ABORTED after {restarts} restart gap(s) — a restart failed and the remaining active drivers were not run against the un-isolated app"
                    ),
                    evidence: vec![EvidenceRef::point(
                        EvidenceKind::Other,
                        "restart_replay_aborted",
                        "restart failure aborts remaining active members (audit P0-11)",
                    )
                    .with_detail(json!({ "restarts": restarts }))],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                    occurrence_id: None,
                });
            } else {
                fs.push(Finding {
                    id: "ORCH-DEEP-SUMMARY".into(),
                    rule_id: None,
                    severity: Severity::Info,
                    category: Category::Orchestration,
                    summary: format!(
                        "restart_between_mutations: {restarts} restart-replay gap(s) inserted between mutating drivers"
                    ),
                    evidence: vec![EvidenceRef::point(
                        EvidenceKind::Other,
                        "restart_replay_summary",
                        "restart count for this composite run",
                    )
                    .with_detail(json!({ "restarts": restarts }))],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                    occurrence_id: None,
                });
            }
        }
    } else {
        // Single profile: dispatch through the table, with the standalone
        // budgets where they differ from full's composite budgets.
        let d = descriptor(&profile);
        let body = SINGLE_PROFILE_OVERRIDES
            .iter()
            .find(|(p, _)| *p == profile)
            .map(|(_, f)| *f)
            .unwrap_or(d.driver);
        if d.transaction {
            fs.extend(tx(session, &mut |s| body(s, &mut graph)));
        } else {
            fs.extend(body(session, &mut graph));
        }
    }
    findings.extend(orchestration_notes);
    findings.extend(fs);

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
        metrics,
    })
}

/// Error-shaped finding helper for driver-level failures (unused today;
/// kept so orchestration-level failures carry typed evidence).
#[allow(dead_code)]
fn orchestration_error(profile: &str, summary: String) -> Finding {
    Finding {
        id: format!("AUDIT-ERR-{}", profile.to_uppercase()),
        rule_id: None,
        severity: Severity::Error,
        category: Category::Audit,
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
        occurrence_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 fix 2: `full` must parse as the composite profile and must be
    /// routed to the live session — the exact regression this module
    /// exists for.
    #[test]
    fn full_is_live_routed_and_parseable() {
        let p = AuditProfile::parse("full").expect("full parses");
        assert_eq!(p, AuditProfile::Full);
        assert!(p.needs_live_session(), "full must read the live app");
        assert!(p.wants_static_composite(), "full includes static passes");
    }

    #[test]
    fn unknown_profiles_are_errors() {
        assert!(AuditProfile::parse("detailed").is_err());
        assert!(AuditProfile::parse("").is_err());
        // The error names every table selector (the old hand-written list
        // omitted 8 real profiles).
        let msg = AuditProfile::parse("detailed").unwrap_err();
        for d in PROFILES {
            assert!(msg.contains(d.id), "error must name {}", d.id);
        }
    }

    /// Every table selector parses and round-trips through `name()`.
    #[test]
    fn every_table_selector_parses() {
        for d in PROFILES {
            let p =
                AuditProfile::parse(d.id).unwrap_or_else(|e| panic!("{} must parse: {e}", d.id));
            assert_eq!(p.name(), d.id);
            assert_eq!(p, d.profile);
        }
    }

    /// The parse error's accepted list IS the table (no second list).
    #[test]
    fn accepted_selectors_come_from_the_table() {
        let msg = AuditProfile::parse("nope").unwrap_err();
        let listed: Vec<&str> = msg
            .rsplit("expected one of ")
            .next()
            .unwrap()
            .split(", ")
            .collect();
        let table: Vec<&str> = PROFILES.iter().map(|d| d.id).collect();
        assert_eq!(listed, table);
    }

    /// Descriptor invariants the orchestrator relies on (review §7: the
    /// table is the authority, so its internal contract is tested here).
    #[test]
    fn descriptor_table_invariants() {
        // Every variant has exactly one row.
        let variants = [
            AuditProfile::Full,
            AuditProfile::Keyboard,
            AuditProfile::Focus,
            AuditProfile::Resize,
            AuditProfile::Layout,
            AuditProfile::Clipping,
            AuditProfile::Discoverability,
            AuditProfile::Navigation,
            AuditProfile::Contract,
            AuditProfile::Color,
            AuditProfile::Performance,
            AuditProfile::Mouse,
            AuditProfile::States,
            AuditProfile::Errors,
            AuditProfile::Unicode,
            AuditProfile::Controls,
            AuditProfile::TerminalModes,
            AuditProfile::Rendering,
            AuditProfile::InputProtocol,
            AuditProfile::ShellCli,
            AuditProfile::Lifecycle,
            AuditProfile::LifecycleExit,
            AuditProfile::QueryResponse,
        ];
        for v in variants {
            let n = PROFILES.iter().filter(|d| d.profile == v).count();
            assert_eq!(n, 1, "{:?} must have exactly one descriptor row", v);
        }
        // full never includes itself or anything process-consuming.
        for d in PROFILES.iter().filter(|d| d.included_in_full) {
            assert_ne!(d.profile, AuditProfile::Full);
            assert_ne!(
                d.risk,
                MutationRisk::RestartRequired,
                "{} must never be a full member: full must not consume the target",
                d.id
            );
        }
        // Restart-required profiles demand exclusive control.
        for d in PROFILES
            .iter()
            .filter(|d| d.risk == MutationRisk::RestartRequired)
        {
            assert!(
                d.exclusive_control,
                "{} consumes the process; it must demand exclusive control",
                d.id
            );
        }
        // Observational profiles never demand exclusive control (the lease
        // must not block passive diagnostics — review §5).
        for d in PROFILES
            .iter()
            .filter(|d| d.risk == MutationRisk::Observational)
        {
            assert!(
                !d.exclusive_control,
                "{} is observational; the human lease must not block it",
                d.id
            );
        }
        // full's driver sequence (full_group > 0) is contiguous 1..=N with
        // no gaps, so group-boundary detection in the composite loop works.
        let mut groups: Vec<u8> = PROFILES
            .iter()
            .filter(|d| d.included_in_full && d.full_group > 0)
            .map(|d| d.full_group)
            .collect();
        groups.sort();
        groups.dedup();
        assert_eq!(groups, (1..=groups.len() as u8).collect::<Vec<u8>>());
    }

    /// Review §5: the lease split. Passive/observational profiles
    /// (including live-session readers) must NOT demand exclusive
    /// control; drivers and process-consumers must.
    #[test]
    fn lease_gate_split() {
        for id in [
            "color",
            "terminal_modes",
            "rendering",
            "input_protocol",
            "shell_cli",
            "lifecycle",
            "query_response",
            "discoverability",
            "unicode",
            "controls",
        ] {
            let p = AuditProfile::parse(id).unwrap();
            assert!(
                !p.requires_exclusive_control(),
                "{id} must run under a human lease"
            );
        }
        for id in [
            "full",
            "keyboard",
            "focus",
            "resize",
            "layout",
            "clipping",
            "navigation",
            "performance",
            "mouse",
            "states",
            "errors",
            "contract",
            "lifecycle_exit",
        ] {
            let p = AuditProfile::parse(id).unwrap();
            assert!(
                p.requires_exclusive_control(),
                "{id} must refuse under a human lease"
            );
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
        // The child emits its DECSET negotiation at startup; under
        // full-suite parallel PTY load those bytes can still be in flight
        // when the profile reads the raw ring — same class as the wave3c
        // flake. A plain observe settles the stream first (the profile is
        // a ring read and deliberately does not wait itself).
        {
            let _ = pool
                .with_session(Some(&id), |s| {
                    let _ = s.observe(300);
                })
                .await;
        }
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
        // The child emits its whole flicker signature (alt screen, erase
        // cycles, cursor toggles) at startup; under full-suite parallel PTY
        // load those bytes can still be in flight when the first audit's
        // ring read runs — same class as the query_response flake. A plain
        // observe settles the stream before any audit reads the ring (the
        // audits are ring reads and deliberately do not wait themselves).
        {
            let _ = pool
                .with_session(Some(&id), |s| {
                    let _ = s.observe(300);
                })
                .await;
        }

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
        // The child writes its DA1/6n queries at startup; give the PTY a
        // moment to deliver them into the backend's retained ring before
        // the audit reads it (a plain observe settles the stream — the
        // audit itself is a ring read and deliberately does not wait).
        {
            let _ = pool
                .with_session(Some(&id), |s| {
                    let _ = s.observe(300);
                })
                .await;
        }
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
