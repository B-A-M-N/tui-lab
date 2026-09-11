//! Terminal capability profile + Explain surface (Wave G review P1/P2 16).
//!
//! Two connective primitives the audit and the agent were missing:
//!
//! * **[`TerminalProfile`]** — a derived, *evidence-backed* answer to "what
//!   can I actually rely on this terminal doing?" It lifts the backend's flat
//!   [`Capabilities`] booleans into per-feature entries that each carry the
//!   concrete observation that proved (or failed to prove) the capability —
//!   mirroring the codebase's core rule that inference is never presented as
//!   ground truth. A feature we never observed staying supported is reported
//!   as `Unsupported` with an honest "not observed" reason, so the agent
//!   doesn't assume mouse or kitty or title just because it *might* exist.
//!
//! * **`FindingExplanation`** — the "Explain" surface: ties one audit
//!   [`Finding`](crate::audit::Finding) to the probe and source that produced
//!   it. A finding carries typed `EvidenceRef`s but no story; the explainer
//!   walks each ref, resolves it against the runtime probe/transition data
//!   available, and emits a path-shaped explanation ("this finding came from
//!   probe `P`, whose before→after transition added 3 controls; the source is
//!   frame hash `H`"). This is what lets the agent trust or challenge a
//!   finding instead of taking it on faith.

pub mod explain;

use crate::backend::{Capabilities, EventCapability, InputFamily, WaitCapability};

/// One terminal capability's detection verdict, with evidence.
///
/// The truthful tri-state mirrors [`Capabilities`]'s negotiation model: a
/// capability we observed negotiate is `Supported`; one we observed being
/// *refused* is `Unsupported`; one we simply never saw either way is
/// `Unverified`. `Unverified` is not "off" — it is "don't assume".
/// Typed provenance for one profile row (audit findings 32–34): only an
/// observation from the CURRENT session may promote a runtime capability.
/// Historical/foreign facts remain visible but never become this session's
/// verdict.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CapabilityEvidence {
    /// Observed on this session/generation (or provided by the backend's
    /// current descriptor).
    CurrentSession {
        generation: Option<u64>,
        observation: String,
    },
    /// Provenance from another run/process. Display-only.
    Historical {
        run_id: Option<String>,
        observation: String,
    },
    /// No usable runtime fact.
    NotObserved,
}

impl CapabilityEvidence {
    /// May this evidence promote a current-session feature?
    pub fn is_current(&self) -> bool {
        matches!(self, CapabilityEvidence::CurrentSession { .. })
    }

    /// Display text (factual; never an assertion).
    pub fn observation(&self) -> &str {
        match self {
            CapabilityEvidence::CurrentSession { observation, .. }
            | CapabilityEvidence::Historical { observation, .. } => observation,
            CapabilityEvidence::NotObserved => "not observed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// Observed negotiated at runtime — the capability is real and usable.
    Supported,
    /// Observed refused / known absent (e.g. non-Unix signals).
    Unsupported,
    /// Never observed either way — usable only after an explicit capability
    /// probe, never assumed.
    Unverified,
}

impl CapabilityState {
    pub fn name(&self) -> &'static str {
        match self {
            CapabilityState::Supported => "supported",
            CapabilityState::Unsupported => "unsupported",
            CapabilityState::Unverified => "unverified",
        }
    }
}

/// A transport-independent terminal persona (P1-49/50): what a *terminal*
/// claims to be, independent of how the child is hosted. A persona is a
/// declared compatibility input — transport capabilities remain evidence in
/// [`Capabilities`], while these declarations describe the environment the
/// target observes (TERM/COLORTERM, color depth, protocol expectations).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalPersona {
    /// Stable id, e.g. `"xterm-256color"`, `"kitty"`, `"dumb"`.
    pub id: String,
    /// `TERM` the child observes.
    pub term: String,
    /// `COLORTERM`, when the persona declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colorterm: Option<String>,
    /// Declared color depth: `none`, `16`, `256`, or `truecolor`.
    pub color_depth: String,
    /// Declared protocol support (by stable ids). These are *persona
    /// declarations*; runtime evidence may still promote/deny them in
    /// profile rows.
    pub supports: Vec<TerminalPersonaFeature>,
    /// Environment pairs a launch using this persona must inject.
    pub env: Vec<(String, String)>,
}

/// Stable persona feature vocabulary (P1-49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPersonaFeature {
    ApplicationCursor,
    AlternateScreen,
    BracketedPaste,
    Mouse,
    MouseSgr,
    KittyKeyboard,
    Osc8,
    SynchronizedUpdates,
    QueryResponses,
}

impl TerminalPersonaFeature {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ApplicationCursor => "application_cursor",
            Self::AlternateScreen => "alternate_screen",
            Self::BracketedPaste => "bracketed_paste",
            Self::Mouse => "mouse",
            Self::MouseSgr => "mouse_sgr",
            Self::KittyKeyboard => "kitty_keyboard",
            Self::Osc8 => "osc8",
            Self::SynchronizedUpdates => "synchronized_updates",
            Self::QueryResponses => "query_responses",
        }
    }
}

impl TerminalPersona {
    /// The canonical built-in personas for matrix runs.
    pub fn builtin() -> Vec<Self> {
        vec![
            Self {
                id: "dumb".into(),
                term: "dumb".into(),
                colorterm: None,
                color_depth: "none".into(),
                supports: vec![],
                env: vec![("TERM".into(), "dumb".into())],
            },
            Self {
                id: "xterm".into(),
                term: "xterm".into(),
                colorterm: None,
                color_depth: "16".into(),
                supports: vec![
                    TerminalPersonaFeature::ApplicationCursor,
                    TerminalPersonaFeature::AlternateScreen,
                ],
                env: vec![("TERM".into(), "xterm".into())],
            },
            Self {
                id: "xterm-256color".into(),
                term: "xterm-256color".into(),
                colorterm: None,
                color_depth: "256".into(),
                supports: vec![
                    TerminalPersonaFeature::ApplicationCursor,
                    TerminalPersonaFeature::AlternateScreen,
                    TerminalPersonaFeature::Osc8,
                ],
                env: vec![("TERM".into(), "xterm-256color".into())],
            },
            Self {
                id: "truecolor".into(),
                term: "xterm-256color".into(),
                colorterm: Some("truecolor".into()),
                color_depth: "truecolor".into(),
                supports: vec![
                    TerminalPersonaFeature::ApplicationCursor,
                    TerminalPersonaFeature::AlternateScreen,
                    TerminalPersonaFeature::BracketedPaste,
                    TerminalPersonaFeature::Osc8,
                    TerminalPersonaFeature::SynchronizedUpdates,
                    TerminalPersonaFeature::QueryResponses,
                ],
                env: vec![
                    ("TERM".into(), "xterm-256color".into()),
                    ("COLORTERM".into(), "truecolor".into()),
                ],
            },
        ]
    }

    /// The launch env pairs for this persona (persona env wins over caller
    /// pairs for the keys it owns).
    pub fn apply_env(&self, env: &mut Vec<(String, String)>) {
        for (k, v) in &self.env {
            if let Some(pair) = env.iter_mut().find(|(ek, _)| ek == k) {
                pair.1 = v.clone();
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
    }

    /// Whether this persona declares a feature.
    pub fn declares(&self, feature: TerminalPersonaFeature) -> bool {
        self.supports.contains(&feature)
    }
}

/// Differential result for running the same scenario/check across personas
/// (P1-49): the persona declaration plus the outcome's stable JSON payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersonaMatrixRow<T> {
    pub persona: String,
    pub term: String,
    pub colorterm: Option<String>,
    pub outcome: T,
}

/// Finding 50: one independently relaunched execution of the same scenario
/// under exactly one persona. The launch fingerprint and scenario identity
/// make accidental cross-contamination observable instead of silently
/// comparing different products.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersonaMatrixOutcome<T> {
    pub row: PersonaMatrixRow<T>,
    pub scenario_id: String,
    pub scenario_schema: String,
    pub launch_spec: crate::session::state::LaunchSpec,
    /// Stable semantic payload selected by the caller (often a
    /// [`ScenarioRunReport`]).
    pub generation: u32,
}

/// Final serializable persona-matrix view: collapse a
/// [`PersonaMatrixExecution`] after its adapter has converted launch errors
/// into rows, or otherwise normalized per-persona outcomes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PersonaMatrixReport<T> {
    pub personas_requested: Vec<String>,
    pub outcomes: Vec<PersonaMatrixOutcome<T>>,
    /// True only when every persona launched successfully. Scenario verdicts
    /// stay inside each row; this gate is about harness validity.
    pub launches_passed: bool,
}

impl<T> PersonaMatrixReport<T> {
    /// Build from the runner's execution result, using `fallback` for any
    /// typed adapter error. The fallback makes harness failures explicit in
    /// the machine payload rather than discarding that row.
    pub fn from_execution<E>(
        execution: crate::terminal::PersonaMatrixExecution<T, E>,
        fallback: impl Fn(&E) -> T,
    ) -> Self {
        let personas_requested = execution
            .outcomes
            .iter()
            .map(|outcome| match outcome {
                Ok(row) => row.row.persona.clone(),
                Err(_) => String::new(),
            })
            .collect();
        let mut launches_passed = execution.launches_passed;
        let outcomes = execution
            .outcomes
            .into_iter()
            .map(|outcome| match outcome {
                Ok(row) => row,
                Err(error) => {
                    launches_passed = false;
                    crate::terminal::PersonaMatrixOutcome {
                        row: crate::terminal::PersonaMatrixRow {
                            persona: String::new(),
                            term: String::new(),
                            colorterm: None,
                            outcome: fallback(&error),
                        },
                        scenario_id: String::new(),
                        scenario_schema: String::new(),
                        launch_spec: crate::session::state::LaunchSpec::new("", 80, 24),
                        generation: 0,
                    }
                }
            })
            .collect();
        Self {
            personas_requested,
            outcomes,
            launches_passed,
        }
    }
}

/// Runner-level matrix result. Each entry is either a fully provenanced
/// outcome or the environment adapter's typed launch/execution error. Cleanup
/// ids are returned separately so async pool owners can stop sessions after
/// the non-async runner has assembled the result.
#[derive(Debug, Clone)]
pub struct PersonaMatrixExecution<T, E> {
    pub outcomes: Vec<Result<PersonaMatrixOutcome<T>, E>>,
    pub cleanup_ids: Vec<String>,
    pub launches_passed: bool,
}

/// One row of a [`TerminalProfile`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfileFeature {
    /// Stable feature id, e.g. `"mouse"`, `"bracketed_paste"`.
    pub id: &'static str,
    /// Human-readable feature name.
    pub name: &'static str,
    /// Detection verdict.
    pub state: CapabilityState,
    /// What the agent can concretely *do* with and without the feature.
    pub implications: &'static str,
    /// Typed evidence for this row (audit findings 32–34): scoped so a
    /// prior run cannot promote a current capability.
    pub evidence: CapabilityEvidence,
    /// Recording fidelity, when the row describes recording/capture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_fidelity: Option<String>,
}

/// An evidence-backed inventory of what the current terminal actually
/// negotiated (Wave G review P1/P2 16). Built from a live
/// [`Capabilities`] snapshot plus per-feature observed-evidence facts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TerminalProfile {
    /// Categorical answer: does this terminal support the feature set the
    /// current UI likely needs? Derived from the feature rows, never stored.
    pub verdict: String,
    /// One row per capability surveyed.
    pub features: Vec<ProfileFeature>,
    /// The raw capabilities this profile was built from (for the explainer
    /// to cite).
    pub source: Capabilities,
}

impl TerminalProfile {
    /// Build a profile from a live [`Capabilities`] snapshot and the observed
    /// per-feature evidence strings the backend logged (see the backend's
    /// `capabilities()` — it tracks the concrete proof per capability).
    ///
    /// Capabilities fall into three honesty classes, which decide the verdict:
    ///
    /// * **Negotiation-promoted** (`mouse`, `kitty_keyboard`, `title`,
    ///   `scrollback`, `bracketed_paste`) — a `true` flag means the running
    ///   application was observed negotiating it, so that IS the evidence. A
    ///   `false` flag means we *never observed it* → `Unverified`, never
    ///   `Unsupported`: absence of observation is not refusal, and assuming
    ///   one way or the other is exactly the mistake the profile exists to
    ///   prevent.
    /// * **Always-on baseline** (`colors`, `cell_attributes`) — genuine in any
    ///   terminal the backend can parse; scored `Supported` with the backend
    ///   default as the cited evidence.
    /// * **Platform truth** (`signals`) — `cfg!(unix)`, not an observation.
    ///
    /// Build a current-session profile. The optional map is compatibility
    /// input: every string is classified as HISTORICAL and can annotate the
    /// row, but cannot promote the current flag (audit finding 32). Process
    /// ownership is reported categorically (finding 33), and recording rows
    /// carry the backend's fidelity (finding 34).
    pub fn build(
        caps: Capabilities,
        evidence: &std::collections::HashMap<&'static str, String>,
    ) -> Self {
        // Per-capability: (id, name, flag, is_baseline, implications).
        // audit finding 37: the row set now covers operation-oriented facts
        // (raw input, bell/exit-code observability, shell integration, stdout/
        // stderr separation, recording, native semantics, attach, query/response)
        // plus representative event/wait/input-coverage rows read from the real
        // list fields — every row reads a REAL Capabilities field, never an
        // intent.
        let rows: Vec<(&'static str, &'static str, bool, bool, &'static str)> = vec![
            (
                "mouse",
                "Mouse",
                caps.mouse,
                false,
                "with it: click/drag targeting; without: keyboard-only controls, coordinate targets unverifiable",
            ),
            (
                "kitty_keyboard",
                "Kitty keyboard protocol",
                caps.kitty_keyboard,
                false,
                "with it: extended key reporting; without: modifiers beyond the base keyset are unverifiable",
            ),
            (
                "colors",
                "Color / attributes",
                caps.colors,
                false,
                "with it: color/style evidence is trustworthy; without: treat style as absent",
            ),
            (
                "cell_attributes",
                "Cell attributes",
                caps.cell_attributes,
                false,
                "with it: style diffs are reliable; without: style inference is unreliable",
            ),
            (
                "title",
                "Terminal title",
                caps.title,
                false,
                "with it: title reads and title-setting actions work; without: title is unset/blank",
            ),
            (
                "scrollback",
                "Scrollback capture",
                caps.scrollback,
                false,
                "with it: historical lines are retrievable; without: only the live viewport is available",
            ),
            (
                "bracketed_paste",
                "Bracketed paste",
                caps.bracketed_paste,
                false,
                "with it: injected text is distinguishable from typed; without: paste and typing collapse",
            ),
            (
                "signals",
                "POSIX signals",
                caps.signals,
                false,
                "with it: ctrl+c / kill / SIGTERM are reliable; without: process teardown is best-effort",
            ),
            (
                "protocol_capture",
                "Raw protocol capture",
                caps.protocol_capture,
                false,
                "with it: protocol-byte diagnosis is available; without (e.g. tmux attach): only rendered panes are observable",
            ),
            // --- audit finding 37: operation-oriented rows ---
            (
                "raw_input",
                "Raw input",
                caps.raw_input,
                false,
                "with it: arbitrary bytes can be injected; without (e.g. tmux): raw escapes degrade to text or are refused",
            ),
            (
                "bell_observable",
                "Bell observability",
                caps.bell_observable,
                false,
                "with it: a BEL is detectable via a Bell wait; without: bell-driven waits cannot resolve",
            ),
            (
                "exit_code",
                "Process exit code",
                caps.exit_code,
                false,
                "with it: the child's real exit code is reportable; without (e.g. tmux attach): only running/dead is known",
            ),
            (
                "shell_integration",
                "Shell integration (OSC 133)",
                caps.shell_integration,
                false,
                "with it: command-phase waits and command_state() work; without: command waits are unsatisfiable",
            ),
            (
                "stdout_stderr_separation",
                "stdout/stderr separation",
                caps.stdout_stderr_separation,
                false,
                "with it: stdout and stderr are readable separately; without: streams are fused",
            ),
            (
                "recording",
                "Recording / cast capture",
                caps.recording,
                false,
                "fidelity determines what survives: raw PTY, separated streams, or reconstructed snapshots",
            ),
            (
                "native_semantic",
                "Native semantic protocol",
                caps.native_semantic,
                false,
                "with it: the app's native side-channel events are absorbable; without: only inferred semantics",
            ),
            (
                "attach",
                "Attach semantics",
                caps.attach,
                false,
                "with it: an existing TUI is attached (tmux); without: the session spawns its own child",
            ),
            (
                "process_ownership",
                "Process ownership",
                !matches!(caps.process_ownership, crate::backend::ProcessOwnership::Unknown),
                false,
                "spawned_child: the session can signal the process and trust its exit code; attached: valid categorical ownership with observe-only authority",
            ),
            (
                "query_response",
                "Query/response probing",
                caps.query_response,
                false,
                "with it: device-query round trips are observable; without: query diagnosis is unavailable",
            ),
            (
                "event_raw",
                "Raw event observability",
                caps.event_types.contains(&EventCapability::Raw),
                false,
                "with it: raw-protocol events are emitted; without: only rendered output is observable",
            ),
            (
                "wait_title",
                "Title waits",
                caps.supported_waits.contains(&WaitCapability::Title),
                false,
                "with it: WaitCond::Title resolves; without: title waits time out",
            ),
            (
                "wait_command",
                "Command waits",
                caps.supported_waits
                    .contains(&WaitCapability::CommandDone),
                false,
                "with it: WaitCond::CommandDone/CommandOutput resolve; without: command waits error/time out",
            ),
            (
                "input_mouse",
                "Mouse injection",
                caps.input_families.contains(&InputFamily::Mouse),
                false,
                "with it: mouse events can be injected; without: mouse driving is unavailable",
            ),
            (
                "input_signal",
                "Signal injection",
                caps.input_families.contains(&InputFamily::Signal),
                false,
                "with it: POSIX signals can be delivered to the child; without: teardown is best-effort",
            ),
            (
                "input_raw",
                "Raw input injection",
                caps.input_families.contains(&InputFamily::RawByte),
                false,
                "with it: raw bytes can be injected; without: raw injection is refused",
            ),
        ];

        // Audit finding 34: recording provenance is a first-class
        // capability, not an ad-hoc display string. Preserve the backend
        // transport mode and sampling interval for later consumers.
        let recording_fidelity = caps.observability_fidelity.as_ref().map(|f| {
            serde_json::json!({
                "mode": f.mode,
                "sampling_ms": f.sampling_ms,
                "blind_spots": f.blind_spots,
            })
            .to_string()
        });
        let features: Vec<ProfileFeature> = rows
            .iter()
            .map(|(id, name, flag, is_baseline, implications)| {
                // Historical facts are display-only (finding 32). They can
                // annotate the row but cannot change the CURRENT verdict.
                let historical =
                    evidence
                        .get(id)
                        .map(|observation| CapabilityEvidence::Historical {
                            run_id: None,
                            observation: observation.clone(),
                        });
                let typed_evidence = if *flag {
                    CapabilityEvidence::CurrentSession {
                        generation: None,
                        observation: observed_evidence_for(id),
                    }
                } else if *is_baseline {
                    CapabilityEvidence::CurrentSession {
                        generation: None,
                        observation: format!(
                            "baseline {} support (backend default)",
                            name.to_lowercase()
                        ),
                    }
                } else {
                    historical.unwrap_or(CapabilityEvidence::NotObserved)
                };
                let observed_flag = *flag || typed_evidence.is_current();
                let state = if observed_flag {
                    CapabilityState::Supported
                } else if *is_baseline {
                    CapabilityState::Supported
                } else if matches!(
                    *id,
                    "mouse" | "kitty_keyboard" | "title" | "scrollback" | "bracketed_paste"
                ) {
                    CapabilityState::Unverified
                } else {
                    CapabilityState::Unsupported
                };
                ProfileFeature {
                    id,
                    name,
                    state,
                    implications,
                    evidence: typed_evidence,
                    recording_fidelity: if *id == "recording" {
                        recording_fidelity.clone()
                    } else {
                        None
                    },
                }
            })
            .collect();

        let verdict = overall_verdict(&features);
        TerminalProfile {
            verdict,
            features,
            source: caps,
        }
    }
}

impl Default for TerminalProfile {
    fn default() -> Self {
        TerminalProfile::build(Capabilities::default(), &std::collections::HashMap::new())
    }
}

/// The concrete runtime observation that proved a negotiation-promoted
/// capability, keyed by feature id. Factual, never an assertion about the
/// terminal's general behavior.
fn observed_evidence_for(id: &str) -> String {
    match id {
        "mouse" => "mouse protocol mode observed".to_string(),
        "kitty_keyboard" => "kitty keyboard flags observed".to_string(),
        "title" => "OSC title sequence observed".to_string(),
        "scrollback" => "scrollback rows captured".to_string(),
        "bracketed_paste" => "bracketed-paste negotiation observed".to_string(),
        _ => "runtime observation on record".to_string(),
    }
}

/// One-line categorical read of the profile.
fn overall_verdict(features: &[ProfileFeature]) -> String {
    let supported = features
        .iter()
        .filter(|f| f.state == CapabilityState::Supported)
        .count();
    let unverified = features
        .iter()
        .filter(|f| f.state == CapabilityState::Unverified)
        .count();
    if supported == features.len() {
        "all surveyed capabilities negotiated — full baseline".to_string()
    } else if unverified > 0 {
        format!(
            "{supported} of {} capabilities confirmed; {unverified} unverified — \
             probe before relying on them",
            features.len(),
        )
    } else {
        format!(
            "{supported} of {} capabilities confirmed; the rest are absent/refused",
            features.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn feat<'a>(p: &'a TerminalProfile, id: &str) -> &'a ProfileFeature {
        p.features
            .iter()
            .find(|f| f.id == id)
            .expect("feature present")
    }

    #[test]
    fn default_profile_is_honest_about_unobserved_capabilities() {
        let p = TerminalProfile::default();
        // Negotiation-promoted never observed → Unverified, never assumed.
        assert_eq!(feat(&p, "mouse").state, CapabilityState::Unverified);
        assert_eq!(feat(&p, "title").state, CapabilityState::Unverified);
        assert_eq!(feat(&p, "scrollback").state, CapabilityState::Unverified);
        assert_eq!(
            feat(&p, "bracketed_paste").state,
            CapabilityState::Unverified
        );
        assert_eq!(
            feat(&p, "kitty_keyboard").state,
            CapabilityState::Unverified
        );
        // Capability defaults make no claim: a false flag is unavailable,
        // not merely unobserved.
        assert_eq!(feat(&p, "colors").state, CapabilityState::Unsupported);
        assert_eq!(
            feat(&p, "cell_attributes").state,
            CapabilityState::Unsupported
        );
        assert_eq!(feat(&p, "signals").state, CapabilityState::Unsupported);
        // The verdict surfaces the unverified count.
        assert!(p.verdict.contains("unverified"));
    }

    #[test]
    fn observed_negotiations_flip_features_to_supported() {
        let caps = Capabilities {
            mouse: true,
            kitty_keyboard: true,
            title: true,
            scrollback: true,
            bracketed_paste: true,
            ..Capabilities::default()
        };
        let p = TerminalProfile::build(caps, &HashMap::new());
        assert_eq!(feat(&p, "mouse").state, CapabilityState::Supported);
        assert_eq!(feat(&p, "title").state, CapabilityState::Supported);
        match &feat(&p, "mouse").evidence {
            CapabilityEvidence::CurrentSession { observation, .. } => assert!(
                observation.contains("mouse protocol mode observed"),
                "a supported capability must cite the observation that proved it: {observation}"
            ),
            other => panic!("mouse must be current-session evidence, got {other:?}"),
        }
    }

    #[test]
    fn foreign_evidence_overrides_a_false_flag() {
        let mut ev = HashMap::new();
        ev.insert(
            "scrollback",
            "scrollback rows captured in a prior run".to_string(),
        );
        // caps say scrollback false (this session); the prior-run fact is
        // retained as display evidence but CANNOT promote the capability
        // (audit finding 32).
        let p = TerminalProfile::build(Capabilities::default(), &ev);
        assert_eq!(feat(&p, "scrollback").state, CapabilityState::Unverified);
        match &feat(&p, "scrollback").evidence {
            CapabilityEvidence::Historical { observation, .. } => {
                assert!(observation.contains("prior run"))
            }
            other => panic!("historical evidence must remain visible: {other:?}"),
        }
    }

    #[test]
    fn feature_evidence_is_never_an_assertion() {
        // A feature that is genuinely unverified must say "not observed",
        // never claim it works.
        let p = TerminalProfile::default();
        assert_eq!(
            feat(&p, "mouse").evidence,
            CapabilityEvidence::NotObserved,
            "absence must be stated as absence"
        );
    }
}

#[cfg(test)]
mod persona_tests {
    use super::*;
    use crate::backend::trait_def::TerminalBackend;

    #[test]
    fn builtin_personas_cover_color_and_protocol_ladder() {
        let personas = TerminalPersona::builtin();
        assert_eq!(personas.len(), 4);
        assert_eq!(personas[0].term, "dumb");
        assert_eq!(personas[3].color_depth, "truecolor");
        assert!(personas[3].declares(TerminalPersonaFeature::SynchronizedUpdates));
        assert!(!personas[0].declares(TerminalPersonaFeature::Osc8));
    }

    #[test]
    fn persona_declares_terminal_behavior_contract() {
        let persona = TerminalPersona::builtin()
            .into_iter()
            .find(|p| p.id == "truecolor")
            .expect("truecolor persona");
        assert!(persona.declares(TerminalPersonaFeature::SynchronizedUpdates));

        let mut backend = crate::backend::PortablePtyBackend::new(20, 6);
        assert!(!backend.capabilities().synchronized_updates);
        backend.apply_terminal_persona(&persona);
        let caps = backend.capabilities();
        assert!(caps.synchronized_updates);
        assert!(caps.query_response);
        // The current persona intentionally does not claim OSC 8 until the
        // emulator implements its lifecycle, so env text cannot fabricate it.
        assert!(!caps.osc8);
    }

    #[test]
    fn persona_env_overrides_only_owned_keys() {
        let persona = TerminalPersona::builtin().remove(3);
        let mut env = vec![
            ("TERM".to_string(), "vt100".to_string()),
            ("MY_VAR".to_string(), "keep".to_string()),
        ];
        persona.apply_env(&mut env);
        assert!(env
            .iter()
            .any(|(k, v)| k == "TERM" && v == "xterm-256color"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "COLORTERM" && v == "truecolor"));
        assert!(env.iter().any(|(k, v)| k == "MY_VAR" && v == "keep"));
    }
}
