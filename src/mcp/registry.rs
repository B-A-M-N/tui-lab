//! The capability registry (Wave G item 68): the single declaration of what
//! the MCP surface can do, as data.
//!
//! Before this module the tool vocabulary lived in three places that could
//! drift: the `#[tool]` descriptions, the handler match arms, and the
//! hand-written README/SKILL tool lists ("12 tools" while `tui_run` made
//! 13). The registry derives from the same typed selector enums the
//! handlers dispatch on (`crate::mcp::params`), and the skill document's
//! tool section is generated from it — the doc-drift class is closed by
//! construction, and a test pins registry names to the rmcp router's
//! `list_tools` names.

use crate::mcp::params::EnumVariants;
use serde_json::json;

/// One tool's declared capability surface.
pub struct ToolCapability {
    pub name: &'static str,
    pub summary: &'static str,
    /// The selector's name ("action" / "mode" / "condition" / "assertion" /
    /// "profile" / "format") paired with its accepted values, when the tool
    /// has one.
    pub selector: Option<(&'static str, &'static [&'static str])>,
}

/// Every tool, in stable order. `tui_act` has no closed selector (its
/// discriminated union IS the schema), so its `selector` is `None`.
pub const TOOLS: &[ToolCapability] = &[
    ToolCapability {
        name: "tui_session",
        summary: "Manage TUI sessions: start, restart, stop, list, status, plus the human control lease (lease/release).",
        selector: Some(("action", <crate::mcp::params::SessionAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_observe",
        summary: "Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state, protocol trace + mode timeline (portable-pty/line engines), pipe stdout/stderr streams, and inspect — the one-call construction view (frame + semantic identity, per-control stable ids/bounds/state/affordances/source loci, native overlay health, loaded-contract violations; target= narrows to one control).",
        selector: Some(("mode", <crate::mcp::params::ObserveMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_act",
        summary: "Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema). Optional `completion` declares how \"done\" means (stable_screen/first_change/any_change/text_appears/text_disappears/process_exit/command_done/bell/semantic_change/may_be_silent/no_wait) so a silent/exit action is never misreported as settled=false.",
        selector: None,
    },
    ToolCapability {
        name: "tui_intent",
        // The verb is an open-ish set (name string or {"verb":"type","text":...}),
        // not a closed selector enum — like `tui_act`.
        summary: "Act by semantic intent: resolve a target (by=id|text|role|focused) + verb (activate/focus/click/toggle/select/open/type) into a focus-secured execution plan; the response names every step and the risk class before anything is sent. execute=true runs the plan; unresolved targets return target_error with structured candidates (details), not prose.",
        selector: None,
    },
    ToolCapability {
        name: "tui_wait",
        summary: "Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.",
        selector: Some(("condition", <crate::mcp::params::WaitCondition as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_probe",
        summary: "Run one small experiment and get EVERYTHING materially different: baseline vs settled after-frame, causal events inside the probe window, transition (with finding-40 control_deltas — per-control WHAT changed, e.g. 'button/save moved x:65→71', not just changed_cells), watched material changes. stimulus {kind:none} = drift probe.",
        selector: Some(("completion", <crate::mcp::params::ProbeCompletion as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_assert",
        summary: "Assert UI facts (text, text_absent, position, focus, focused_not, not_clipped, dimensions, exit_code, region, snapshot, structure, control_exists); unknown assertions are invalid_request (caller error), never assertion_failed (UI failure). `oracle` evaluates the shared Wave E language.",
        selector: Some(("assertion", <crate::mcp::params::AssertAssertion as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_checkpoint",
        summary: "Save and compare named UI state checkpoints (durable under persistent runs).",
        selector: Some(("action", <crate::mcp::params::CheckpointAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_scenario",
        summary: "Record, save, list, export, and replay interaction scenarios (session+generation scoped); regression_asset synthesizes review-gated regression assets (scenario/assertion/contract_rule/viewport_case) from a finding's own evidence (finding 39) — generated/inferred, never auto-run.",
        selector: Some(("action", <crate::mcp::params::ScenarioAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_record",
        summary: "Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures.",
        selector: Some(("format", <crate::mcp::params::RecordFormat as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_explore",
        summary: "Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, and the state graph (modes: random, guided_candidates, semantic, state_graph). Driving: blocked while a human lease is live. Replay of discovered flows is tui_scenario's job.",
        selector: Some(("mode", <crate::mcp::params::ExploreMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_audit",
        summary: "Deterministic UX audits returning evidence-backed findings; `full` is the composite of every non-process-consuming family. label=/compare_to= diff findings across runs. Safe-only default: invasive profiles are withheld (ORCH-GATED) until allow_mutation=true; restart_between_mutations=true restart-replays between mutating drivers (with allow_mutation=true; not an external-side-effect boundary). Driving profiles are blocked while a human lease is live; observational readers stay allowed. lifecycle_exit consumes the target and needs allow_process_restart=true.",
        selector: Some(("profile", <crate::mcp::params::AuditProfile as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_coverage",
        summary: "Coverage: native NSP coverage-event ledger plus the optional tuicov executable (honest Unsupported when absent).",
        selector: Some(("action", <crate::mcp::params::CoverageAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_framework",
        summary: "Framework detection, capability probes, and NativeSemanticProtocol adapter snippets (Ratatui/Textual/Python).",
        selector: Some(("action", <crate::mcp::params::FrameworkAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_run",
        summary: "Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, diagnose (or its alias repair): diagnostic evidence contexts per finding — provenance-tiered source loci, verification plan (targeted checks, replay only with a reproduction), observation-shaped next steps — never edit prescriptions, bundle for ONE finding (context + before/after regression diff), and context (this registry as JSON).",
        selector: Some(("action", <crate::mcp::params::RunAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_contract",
        summary: "Design contracts: load, validate, conformance status, baseline compare (regressions become findings), and scaffold — generate a starter contract from the LIVE observed frame (scaffold_mode=current) or from a bounded SAFE multi-state pass — initial screen, Tab focus walk, Escape, viewport probes (scaffold_mode=explore; lease-gated; every state cited in the scaffold.inferred extension; edit from observation toward intent).",
        selector: Some(("action", <crate::mcp::params::ContractAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_explain",
        // `finding_id` is an open set (any recorded finding), not a closed
        // selector — like `tui_act`.
        summary: "Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities (via the live profile) the finding is conditional on.",
        selector: None,
    },
    ToolCapability {
        name: "tui_workflow",
        summary: "Construction workflow per finding (one object, no autonomy): inspect assembles finding → component identity → source loci → framework context → contract expectation → minimal reproduction → targeted validation; verify runs that verification plan live (replay + re-checks; lease-gated) and reports whether the finding still reproduces; diagnose lists every finding's chain.",
        selector: Some(("action", <crate::mcp::params::WorkflowAction as EnumVariants>::VARIANTS)),
    },
];

/// One declared resource surface (templates + the fixed findings feed).
pub struct ResourceCapability {
    /// URI, with `{placeholders}` for templates.
    pub uri: &'static str,
    pub description: &'static str,
}

/// Every `tui://` resource, in stable order. The live `tui://runs/<id>`
/// entry (this server's own run) is dynamic and not listed here; these are
/// the shapes a client can enumerate without connecting.
pub const RESOURCES: &[ResourceCapability] = &[
    ResourceCapability {
        uri: "tui://runs/{run_id}",
        description:
            "Run status + manifest. Live runs read live state; persisted runs are restored read-only from disk (live=false).",
    },
    ResourceCapability {
        uri: "tui://runs/{run_id}/scenarios",
        description:
            "Saved scenarios in a run (review P1 evidence-addressability): ids, names, step counts, and the per-scenario URI. Live runs read memory+disk; persisted runs are restored read-only.",
    },
    ResourceCapability {
        uri: "tui://runs/{run_id}/scenarios/{scenario_id}",
        description:
            "One scenario by id (or unambiguous name) — the full recorded step list, addressable as evidence.",
    },
    ResourceCapability {
        uri: "tui://runs/{run_id}/transactions",
        description:
            "The declared-replay transaction ledger (bounded retained window + lifetime count). Citable as the run's interaction history.",
    },
    ResourceCapability {
        uri: "tui://runs/{run_id}/transactions/{seq}",
        description:
            "One transaction by ledger seq: action, settle verdict, before/after structure, changed cells, render evidence.",
    },
    ResourceCapability {
        uri: "tui://sessions/{session_id}/semantic",
        description: "Live semantic screen: regions, controls, focus, affordances, components.",
    },
    ResourceCapability {
        uri: "tui://sessions/{session_id}/screen",
        description: "Live screen text + geometry.",
    },
    ResourceCapability {
        uri: "tui://sessions/{session_id}/terminal-profile",
        description:
            "Evidence-backed terminal capability report (review §12): reads the live backend capabilities without forcing a screen settle — observationally pure, unlike the screen-backed views.",
    },
    ResourceCapability {
        uri: "tui://findings",
        description: "Findings accumulated this run (audits, contracts, exploration).",
    },
    ResourceCapability {
        uri: "tui://findings/{finding_id}",
        description:
            "One finding by instance id, rendered as its explanation (review P1 evidence-addressability): evidence refs traced to sources — the tui_explain shape, as a read-only resource.",
    },
];

/// The full registry as a JSON value (`tui_run action=context` payload).
pub fn to_json() -> serde_json::Value {
    json!({
        "tools": TOOLS.iter().map(|t| json!({
            "name": t.name,
            "summary": t.summary,
            "selector": t.selector.map(|(field, values)| json!({
                "field": field,
                "values": values,
            })),
        })).collect::<Vec<_>>(),
        "resources": RESOURCES.iter().map(|r| json!({
            "uri_template": r.uri,
            "description": r.description,
        })).collect::<Vec<_>>(),
        "count": TOOLS.len(),
    })
}

// ── Canonical flows (audit P1.8) ──
//
// `tui_run action=context` pairs the raw registry with task-oriented
// routes through the machinery. Each step is a REAL tool invocation:
// the arguments are built from the actual `mcp::params` types and
// serialized, never handwritten JSON — the same P0.4 rule the
// DiagnosticContexts follow. A request the params type cannot
// represent cannot appear in a flow, so the flows cannot advertise a
// grammar the server rejects.
//
// Session ids are left as placeholders (`$session`) because flows are
// published before any session exists; every other field is a
// genuine default the handler accepts.

/// The placeholder token for "the session id you already hold or just
/// started" — the only non-literal in any flow step.
pub const SESSION_PLACEHOLDER: &str = "$session";

fn step(tool: &str, why: &str, args: serde_json::Value) -> serde_json::Value {
    json!({ "tool": tool, "why": why, "arguments": args })
}

/// debug_existing_tui: attach/observe → understand → diagnose →
/// experiment → baseline → verify. Encodes the real policy: start
/// observationally, only drive after a diagnosis, and always leave a
/// labeled baseline behind for the fix verification.
fn flow_debug_existing_tui() -> serde_json::Value {
    use crate::mcp::params::*;
    json!({
        "name": "debug_existing_tui",
        "description": "Investigate a misbehaving or unfamiliar TUI: observe first, diagnose from evidence, experiment causally, then keep a baseline so the fix can be proven.",
        "steps": [
            step("tui_session", "start (or attach to an existing tmux pane) and hold the session id",
                 serde_json::to_value(TuiSessionParams {
                     action: "start".into(), command: Some("<command>".into()),
                     args: None, cwd: None, env: None, cols: Some(120), rows: Some(40),
                     backend: None, isolation: None, id: None, holder: None,
                     ttl_ms: None, lease_id: None, target: None,
                 }).unwrap()),
            step("tui_observe", "the one-call construction view: frame identity, semantic controls with stable ids, affordances, contract verdicts",
                 serde_json::to_value(TuiObserveParams {
                     mode: Some(Known::Known(ObserveMode::Inspect)),
                     idle_ms: None, id: Some(SESSION_PLACEHOLDER.into()),
                     consumer: None, query: None, text: None, since_seq: None,
                     until_seq: None, limit: None, event_types: None, target: None,
                 }).unwrap()),
            step("tui_audit", "passive first: observational profiles only — mutating ones are withheld and reported ORCH-GATED",
                 serde_json::to_value(TuiAuditParams {
                     profile: Some("full".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     label: None, compare_to: None, allow_mutation: Some(false),
                     restart_between_mutations: None, allow_process_restart: None,
                 }).unwrap()),
            step("tui_workflow", "the evidence chain for any finding: root cause → minimal reproduction → targeted validation plan",
                 serde_json::to_value(TuiWorkflowParams {
                     action: Known::Known(WorkflowAction::Diagnose),
                     finding_id: None, id: Some(SESSION_PLACEHOLDER.into()),
                     cwd: None, allow_mutation: None,
                 }).unwrap()),
            step("tui_probe", "causal experiment on ONE suspect stimulus — everything materially different, event-scoped",
                 serde_json::to_value(TuiProbeParams {
                     stimulus: None, completion: None, capture: None, text: None,
                     watch: None, quiet_ms: None, budget_ms: None,
                     id: Some(SESSION_PLACEHOLDER.into()),
                 }).unwrap()),
            step("tui_intent", "preview (plan-only) then execute the targeted semantic action the diagnosis pointed at",
                 serde_json::to_value(TuiIntentParams {
                     target: crate::intent::ActionTarget::Focused,
                     verb: IntentVerbParam::Name("activate".into()),
                     id: Some(SESSION_PLACEHOLDER.into()), execute: Some(false),
                     sensitive: None, completion: None, plan_id: None, max_risk: None,
                     no_wait: None, settle_budget_ms: None,
                 }).unwrap()),
            step("tui_audit", "record the pre-fix state under a label — the baseline the fix is diffed against",
                 serde_json::to_value(TuiAuditParams {
                     profile: Some("full".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     label: Some("baseline".into()), compare_to: None,
                     allow_mutation: Some(false), restart_between_mutations: None,
                     allow_process_restart: None,
                 }).unwrap()),
            step("tui_workflow", "after the fix: replay + re-check the finding live and read the verdict against the baseline",
                 serde_json::to_value(TuiWorkflowParams {
                     action: Known::Known(WorkflowAction::Verify),
                     finding_id: Some("$finding_id".into()),
                     id: Some(SESSION_PLACEHOLDER.into()), cwd: None,
                     allow_mutation: Some(false),
                 }).unwrap()),
        ],
    })
}

/// construct_or_refine_tui: detect the framework → scaffold the
/// contract from observation → load/validate → conformance-check
/// (passive) → refine with targeted exploration.
fn flow_construct_or_refine_tui() -> serde_json::Value {
    use crate::mcp::params::*;
    json!({
        "name": "construct_or_refine_tui",
        "description": "Build or refine a TUI against a contract: detect the framework, scaffold candidate invariants from observation, then conform — driving only with explicit authorization.",
        "steps": [
            step("tui_framework", "identify the TUI framework and its adapter/native-channel state (root from cwd)",
                 serde_json::to_value(TuiFrameworkParams {
                     action: Known::Known(FrameworkAction::Detect),
                     cwd: Some("$project_dir".into()), source: None, id: None,
                 }).unwrap()),
            step("tui_contract", "scaffold a candidate contract from observed frames only (current mode drives nothing)",
                 serde_json::to_value(TuiContractParams {
                     action: Known::Known(ContractAction::Scaffold),
                     scaffold_mode: Some(Known::Known(ScaffoldMode::Current)),
                     path: Some("$contract.yaml".into()), id: None,
                     baseline: None, label: None, mode: None, allow_mutation: None,
                 }).unwrap()),
            step("tui_contract", "validate the document's structure and evidence claims before conformance",
                 serde_json::to_value(TuiContractParams {
                     action: Known::Known(ContractAction::Validate),
                     scaffold_mode: None, path: Some("$contract.yaml".into()), id: None,
                     baseline: None, label: None, mode: None, allow_mutation: None,
                 }).unwrap()),
            step("tui_contract", "static/passive conformance first: driving checks are reported UNVERIFIED, not executed",
                 serde_json::to_value(TuiContractParams {
                     action: Known::Known(ContractAction::Status),
                     scaffold_mode: None, path: Some("$contract.yaml".into()),
                     id: Some(SESSION_PLACEHOLDER.into()), baseline: None,
                     label: None, mode: None, allow_mutation: None,
                 }).unwrap()),
            step("tui_observe", "inspect the live frame against the loaded contract — per-control facts + violations",
                 serde_json::to_value(TuiObserveParams {
                     mode: Some(Known::Known(ObserveMode::Inspect)),
                     idle_ms: None, id: Some(SESSION_PLACEHOLDER.into()),
                     consumer: None, query: None, text: None, since_seq: None,
                     until_seq: None, limit: None, event_types: None, target: None,
                 }).unwrap()),
            step("tui_contract", "after refining the app: full conformance at the chosen check mode (this DRIVES — explicit allow_mutation, the same authorization tui_audit requires)",
                 serde_json::to_value(TuiContractParams {
                     action: Known::Known(ContractAction::Status),
                     scaffold_mode: None, path: Some("$contract.yaml".into()),
                     id: Some(SESSION_PLACEHOLDER.into()), baseline: None,
                     label: None, mode: Some("strict".into()), allow_mutation: Some(true),
                 }).unwrap()),
        ],
    })
}

/// regression_test_tui: labeled audits → scenario capture → replay on
/// every change → did-the-fix-hold bundle.
fn flow_regression_test_tui() -> serde_json::Value {
    use crate::mcp::params::*;
    json!({
        "name": "regression_test_tui",
        "description": "Turn a verified-good state into a reusable regression gate: labeled baselines, recorded scenarios, replay diffs, and per-finding hold bundles.",
        "steps": [
            step("tui_audit", "record the known-good audit under a stable label",
                 serde_json::to_value(TuiAuditParams {
                     profile: Some("full".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     label: Some("baseline".into()), compare_to: None,
                     allow_mutation: Some(false), restart_between_mutations: None,
                     allow_process_restart: None,
                 }).unwrap()),
            step("tui_scenario", "begin recording the interaction that must keep working",
                 serde_json::to_value(TuiScenarioParams {
                     action: Known::Known(ScenarioAction::RecordStart),
                     name: Some("critical-path".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     recording_id: None, steps: None, parameters: None,
                     on_failure: None, finding_id: None, asset_type: None,
                 }).unwrap()),
            step("tui_scenario", "finish the recorded flow after driving/assertions; the recorder owns the nonempty step set",
                 serde_json::to_value(TuiScenarioParams {
                     action: Known::Known(ScenarioAction::RecordStop),
                     name: Some("critical-path".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     recording_id: Some("$recording_id".into()), steps: None, parameters: None,
                     on_failure: None, finding_id: None, asset_type: None,
                 }).unwrap()),
            step("tui_run", "persist the run so the baseline + scenario survive the session",
                 serde_json::to_value(TuiRunParams {
                     action: Known::Known(RunAction::Persist),
                     root: None, kill_sessions: None, run_id: None, run_dir: None,
                     finding_id: None, compare_to: None,
                     detach_existing_sessions: None, discard: None,
                 }).unwrap()),
            step("tui_scenario", "after a change: run the recorded path against the new build",
                 serde_json::to_value(TuiScenarioParams {
                     action: Known::Known(ScenarioAction::Run),
                     name: Some("critical-path".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     recording_id: None, steps: None, parameters: None,
                     on_failure: None, finding_id: None, asset_type: None,
                 }).unwrap()),
            step("tui_audit", "re-audit and diff against the baseline: FIXED / NEW / PERSISTING per finding",
                 serde_json::to_value(TuiAuditParams {
                     profile: Some("full".into()), id: Some(SESSION_PLACEHOLDER.into()),
                     label: Some("current".into()), compare_to: Some("baseline".into()),
                     allow_mutation: Some(false), restart_between_mutations: None,
                     allow_process_restart: None,
                 }).unwrap()),
            step("tui_run", "one finding's did-the-change-hold packet: context + verdict + side effects elsewhere",
                 serde_json::to_value(TuiRunParams {
                     action: Known::Known(RunAction::Bundle),
                     root: None, kill_sessions: None, run_id: None, run_dir: None,
                     finding_id: Some("$finding_id".into()),
                     compare_to: Some("baseline".into()),
                     detach_existing_sessions: None, discard: None,
                 }).unwrap()),
        ],
    })
}

/// The canonical flows, in stable order. Each is built from typed
/// request builders — see `flow_debug_existing_tui`.
pub fn flows() -> serde_json::Value {
    json!({
        "debug_existing_tui": flow_debug_existing_tui(),
        "construct_or_refine_tui": flow_construct_or_refine_tui(),
        "regression_test_tui": flow_regression_test_tui(),
        "note": "arguments are serialized from the real parameter types; '$session', '$recording_id', '$finding_id', '$project_dir', '$contract.yaml' are placeholders the caller fills with real ids/paths",
    })
}

/// Render the SKILL.md tool section from the registry (item 69: generation,
/// not hand-maintained prose). The output is stable and alphabetical like
/// the old hand-written list, but it cannot drift from the handlers because
/// the selectors ARE the dispatch types.
pub fn skill_tool_section() -> String {
    let mut out = String::from("## Tools\n\n");
    for t in TOOLS {
        out.push_str(&format!("- `{}` — {}\n", t.name, t.summary));
        if let Some((field, values)) = t.selector {
            out.push_str(&format!("  - {}: {}\n", field, values.join(", ")));
        }
    }
    out
}

/// P1.9: the README's per-tool selector vocabulary, generated — one
/// authoritative declaration (`TOOLS` + the dispatch enums), every doc
/// derived from it. A README `**Actions:**` line that drifted from the
/// enum the handler actually dispatches on is the P1.9 defect class;
/// this generator closes it. Output: for every tool with a selector,
/// `### <tool>` then `**<Field>:** v1, v2, …`.
pub fn readme_selector_section() -> String {
    let mut out = String::from("<!-- BEGIN GENERATED SELECTORS (registry.rs) — regenerate: cargo run -- skill --write-readme -->\n\n");
    for t in TOOLS {
        let Some((field, values)) = t.selector else {
            continue;
        };
        out.push_str(&format!(
            "### {}\n\n**{}:** {}\n\n",
            t.name,
            // Capitalize the selector field for the prose style the README
            // already uses ("**Actions:**", "**Modes:**").
            {
                let mut c = field.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            },
            values.join(", ")
        ));
    }
    out.push_str("<!-- END GENERATED SELECTORS -->\n");
    out
}

/// P1.9: splice [`readme_selector_section`] into a README, replacing
/// whatever currently sits between the generated markers. Returns `None`
/// when the markers are missing — the README must carry the empty
/// generated block before this can maintain it (no inventing structure).
pub fn splice_readme_selectors(doc: &str) -> Option<String> {
    const BEGIN: &str =
        "<!-- BEGIN GENERATED SELECTORS (registry.rs) — regenerate: cargo run -- skill --write-readme -->";
    const END: &str = "<!-- END GENERATED SELECTORS -->";
    let start = doc.find(BEGIN)?;
    let end = doc.find(END)? + END.len();
    if end <= start {
        return None;
    }
    // The generated section ends with exactly one newline; trim it here
    // so repeated runs cannot accumulate blank lines at the seam (the
    // doc's own bytes after the END marker are preserved verbatim).
    let section = readme_selector_section();
    let section = section.trim_end_matches('\n');
    Some(format!("{}{}{}", &doc[..start], section, &doc[end..]))
}

/// Render the SKILL.md resource section from the same RESOURCES table the
/// server advertises — one declaration, three consumers (list_resource_
/// templates stays hand-written where it needs rmcp types; the registry
/// JSON and the skill doc both read this).
pub fn skill_resource_section() -> String {
    let mut out = String::from("## Resources (tui://)\n\n");
    for r in RESOURCES {
        out.push_str(&format!("- `{}` — {}\n", r.uri, r.description));
    }
    out
}

/// Splice the generated Tools and Resources sections into a SKILL.md
/// document, replacing whatever currently sits under those two headings.
/// Everything between a generated section's heading and the next `## `
/// heading (or EOF) is generated content; all other prose is preserved
/// byte-for-byte. This is what `cargo run -- skill --write` applies to the
/// file on disk — the fix loop for the drift the parity test catches,
/// without hand-splicing. Returns `None` when a heading is missing (the
/// caller should not invent structure that was never there).
pub fn splice_skill_sections(doc: &str) -> Option<String> {
    let after_tools = splice_one(doc, "## Tools", &skill_tool_section())?;
    splice_one(
        &after_tools,
        "## Resources (tui://)",
        &skill_resource_section(),
    )
}

/// Replace the section from `heading` to the next `\n## ` (or EOF) with
/// `generated` (which starts with that same heading and ends with a
/// newline), preserving everything before and after. The blank-line
/// separator between the replaced section and the next heading is the
/// document's own convention and is kept as-is.
fn splice_one(doc: &str, heading: &str, generated: &str) -> Option<String> {
    let start = doc.find(heading)?;
    let end = doc[start + heading.len()..]
        .find("\n## ")
        .map(|i| start + heading.len() + i + 1) // keep the newline that opens the next heading
        .unwrap_or(doc.len());
    let body = &doc[start..end];
    // `generated` carries one trailing newline; reproduce any blank line
    // the old body had beyond that.
    let blank_lines = body.len() - body.trim_end_matches('\n').len();
    let mut out = String::with_capacity(doc.len() + generated.len());
    out.push_str(&doc[..start]);
    out.push_str(generated);
    for _ in 1..blank_lines {
        out.push('\n');
    }
    out.push_str(&doc[end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicate_names() {
        let mut names: Vec<_> = TOOLS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate tool names in registry");
    }

    #[test]
    fn every_selector_is_nonempty_and_lowercase() {
        for t in TOOLS {
            if let Some((field, values)) = t.selector {
                assert!(!field.is_empty(), "{}: selector field", t.name);
                assert!(
                    !values.is_empty(),
                    "{}: selector '{}' has no values",
                    t.name,
                    field
                );
                for v in values {
                    assert_eq!(*v, v.to_ascii_lowercase(), "{}: {}", t.name, v);
                }
            }
        }
    }

    #[test]
    fn skill_section_lists_every_tool() {
        let section = skill_tool_section();
        for t in TOOLS {
            assert!(section.contains(t.name), "skill section misses {}", t.name);
        }
    }

    #[test]
    fn registry_names_match_the_rmcp_router() {
        // Item 69's pin: the registry cannot drift from what tools/list
        // actually serves. The router is built from the same #[tool] fns
        // the dispatch macro uses, so equality here means the registry and
        // the wire surface are the same set.
        let router = crate::mcp::TuiLabServer::tool_router();
        let mut served: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let mut declared: Vec<String> = TOOLS.iter().map(|t| t.name.to_string()).collect();
        served.sort();
        declared.sort();
        assert_eq!(declared, served, "registry TOOLS != router tools/list set");
    }

    /// Beta-audit P0.9: the ADVERTISED schema and the ACCEPTED wire shape
    /// must agree per field. The old hand-maintained schema mirror drifted
    /// within weeks (resize/signal lacked the guard field the parser
    /// accepts), so this walks the tui_act oneOf and asserts, per action
    /// variant, that every property the schema names deserializes and —
    /// the drift that actually shipped — every field the PARSER accepts
    /// on a payload is advertised.
    #[test]
    fn act_schema_advertises_every_field_the_parser_accepts() {
        let router = crate::mcp::TuiLabServer::tool_router();
        let tool = router.get("tui_act").unwrap();
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let one_of = schema
            .get("oneOf")
            .and_then(|v| v.as_array())
            .expect("tui_act root is a wrapped oneOf");
        assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"));

        // Tag -> advertised property set.
        let mut advertised: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for v in one_of {
            let action = v["properties"]["action"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("variant missing action const: {v}"))
                .to_string();
            let props: Vec<String> = v["properties"]
                .as_object()
                .expect("variant properties")
                .keys()
                .cloned()
                .collect();
            advertised.insert(action, props);
        }
        let expected_actions: &[&str] = &[
            "key",
            "keys",
            "type",
            "paste",
            "raw",
            "mouse_click",
            "mouse_press",
            "mouse_release",
            "mouse_move",
            "mouse_drag",
            "mouse_scroll",
            "resize",
            "signal",
        ];
        assert_eq!(advertised.len(), expected_actions.len(), "{advertised:?}");

        // Every action accepts the shared transport fields; the schema
        // must advertise them (guard included — the historical drift).
        for action in expected_actions {
            let props = &advertised[*action];
            for shared in ["id", "no_wait", "completion", "wait_ms", "guard"] {
                assert!(
                    props.contains(&shared.to_string()),
                    "{action}: schema must advertise '{shared}' (the mirror dropped guard from resize/signal): {props:?}"
                );
            }
        }

        // And the reverse direction: serialize a maxed request per action
        // (every field set) and check the schema's properties cover every
        // KEY it emits.
        let full = |tag: &str, extra: serde_json::Value| {
            serde_json::json!({
                "action": tag,
                "no_wait": false,
                "completion": "stable_screen",
                "wait_ms": 10,
                "id": "s1",
                "guard": {},
            })
            .as_object()
            .unwrap()
            .iter()
            .chain(extra.as_object().unwrap())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<String, serde_json::Value>>()
        };
        let samples = [
            full("key", serde_json::json!({ "key": "tab" })),
            full("keys", serde_json::json!({ "keys": ["tab"] })),
            full(
                "type",
                serde_json::json!({ "text": "hi", "sensitive": false }),
            ),
            full(
                "paste",
                serde_json::json!({ "paste": "hi", "sensitive": false }),
            ),
            full("raw", serde_json::json!({ "raw": [1] })),
            full(
                "mouse_click",
                serde_json::json!({ "x": 1, "y": 2, "button": "left" }),
            ),
            full("mouse_press", serde_json::json!({ "x": 1, "y": 2 })),
            full("mouse_release", serde_json::json!({ "x": 1, "y": 2 })),
            full("mouse_move", serde_json::json!({ "x": 1, "y": 2 })),
            full("mouse_drag", serde_json::json!({ "x": 1, "y": 2 })),
            full(
                "mouse_scroll",
                serde_json::json!({ "x": 1, "y": 2, "direction": "down" }),
            ),
            full("resize", serde_json::json!({ "cols": 80, "rows": 24 })),
            full("signal", serde_json::json!({ "signal": 15 })),
        ];
        for s in samples {
            // 1. The PARSER accepts it (schema-valid ⇒ wire-valid).
            let req: crate::mcp::params::TuiActRequest =
                serde_json::from_value(serde_json::Value::Object(s.clone()))
                    .unwrap_or_else(|e| panic!("parser rejects its own maxed shape: {e}: {s:?}"));
            // 2. The SCHEMA advertises every key it carries.
            let action = s["action"].as_str().unwrap();
            let props = &advertised[action];
            for k in s.keys() {
                assert!(
                    props.contains(k),
                    "{action}: parser accepts '{k}' but the schema omits it: {props:?}"
                );
            }
            let _ = req;
        }
    }

    /// Wire compatibility: the historical flat tagged shape still
    /// deserializes identically after the payload-struct refactor — a
    /// recorded scenario from before the refactor must replay.
    #[test]
    fn act_wire_shape_is_backward_compatible() {
        let flat = serde_json::json!({
            "action": "type", "text": "hello", "sensitive": true,
            "no_wait": true, "id": "s9", "wait_ms": 25,
            "completion": { "type": "text_appears", "text": "Saved" },
            "guard": { "structure_hash": "abc", "focus_control_id": "#b/ok" },
        });
        let req: crate::mcp::params::TuiActRequest = serde_json::from_value(flat).unwrap();
        match &req {
            crate::mcp::params::TuiActRequest::Type(p) => {
                assert_eq!(p.text, "hello");
                assert_eq!(p.sensitive, Some(true));
                assert_eq!(p.common.id.as_deref(), Some("s9"));
                assert_eq!(p.common.wait_ms, Some(25));
                assert_eq!(p.common.no_wait, Some(true));
                let guard = p.common.guard.as_ref().expect("guard survives");
                assert_eq!(guard.structure_hash.as_deref(), Some("abc"));
                let completion = p.common.completion.as_ref().expect("completion survives");
                assert!(
                    matches!(
                        completion.to_policy(),
                        crate::capture::CompletionPolicy::TextAppears(ref t)
                            if t == "Saved"
                    ),
                    "completion carries its payload through: {completion:?}"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // And it round-trips back to the same flat shape.
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["action"], "type");
        assert_eq!(back["text"], "hello");
        assert_eq!(back["guard"]["structure_hash"], "abc");
        // Accessors read through the payloads.
        assert!(req.sensitive());
        assert_eq!(req.id(), Some("s9"));
        assert!(req.guard().is_some());
        assert!(req.no_wait());
    }

    #[test]
    fn registry_selectors_match_the_wire_schema() {
        // Audit P1-50: names parity was never behavior parity. For every
        // tool declaring a selector, the selector's VALUES must be exactly
        // the enum values the tool's real JSON schema advertises — a
        // summary that names a mode the schema rejects (or omits one it
        // accepts) is the drift class that survived name-only parity. The
        // schema walk finds the selector property under the params struct
        // and compares its `enum` (or anyOf/oneOf const members) to the
        // registry's VARIANTS.
        let router = crate::mcp::TuiLabServer::tool_router();
        for t in TOOLS {
            let Some((field, values)) = t.selector else {
                continue;
            };
            let tool = router
                .get(t.name)
                .unwrap_or_else(|| panic!("router lost {}", t.name));
            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            let selector_schema = find_property(&schema, field)
                .unwrap_or_else(|| panic!("{}: schema has no '{}'", t.name, field));
            // schemars emits shared wrappers as `$ref`s into `$defs`
            // (`Known<T>` is one generic definition, and the property may
            // be an anyOf of [ref, string]); resolve refs recursively so
            // the walk below sees the real variants.
            let defs = schema.get("$defs").cloned().unwrap_or_default();
            let selector_schema = resolve_refs(&selector_schema, &defs);
            let served_values = enum_consts(&selector_schema).unwrap_or_else(|| {
                panic!(
                    "{}: selector '{}' schema carries no enumerable constants: {selector_schema}",
                    t.name, field
                )
            });
            let mut a: Vec<&str> = values.to_vec();
            let mut b: Vec<String> = served_values;
            a.sort_unstable();
            b.sort();
            assert_eq!(
                a, b,
                "{}: registry selector '{}' drifted from the wire schema",
                t.name, field
            );
        }
    }

    /// Recursively resolve `$ref` pointers into `defs` (schemars hoists
    /// shared wrappers like `Known<T>` there); every other value is
    /// rewritten in place so nested anyOf members resolve too.
    fn resolve_refs(v: &serde_json::Value, defs: &serde_json::Value) -> serde_json::Value {
        if let Some(ref_path) = v.get("$ref").and_then(|r| r.as_str()) {
            let def_name = ref_path.trim_start_matches("#/$defs/");
            if let Some(target) = defs.get(def_name) {
                return resolve_refs(target, defs);
            }
        }
        match v {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(k, val)| (k.clone(), resolve_refs(val, defs)))
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(|c| resolve_refs(c, defs)).collect())
            }
            other => other.clone(),
        }
    }

    /// Depth-first search for a property named `field` anywhere in a JSON
    /// schema (params are nested structs; the selector enum may sit one or
    /// two levels down).
    fn find_property(v: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
                    if let Some(found) = props.get(field) {
                        return Some(found.clone());
                    }
                }
                for (_k, child) in map {
                    if let Some(found) = find_property(child, field) {
                        return Some(found);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => items.iter().find_map(|c| find_property(c, field)),
            _ => None,
        }
    }

    /// Extract the string constants a schema offers for one property:
    /// direct `enum`, or `anyOf`/`oneOf` members with `const`/`enum` of a
    /// single string (the shapes schemars emits for untagged Known<T>
    /// enums).
    fn enum_consts(schema: &serde_json::Value) -> Option<Vec<String>> {
        if let Some(vals) = schema.get("enum").and_then(|e| e.as_array()) {
            return Some(
                vals.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            );
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(variants) = schema.get(key).and_then(|v| v.as_array()) {
                let mut out = Vec::new();
                for variant in variants {
                    if let Some(c) = variant.get("const").and_then(|c| c.as_str()) {
                        out.push(c.to_string());
                    } else if let Some(vals) = variant.get("enum").and_then(|e| e.as_array()) {
                        // The Known<T> shape: one anyOf member carries the
                        // FULL variant enum; the other is the free-string
                        // arm (no enum). Collect every string it offers.
                        out.extend(vals.iter().filter_map(|v| v.as_str().map(str::to_string)));
                    } else if let Some(nested) = enum_consts(variant) {
                        // Option<Known<T>> nests a Known anyOf INSIDE the
                        // Option anyOf — recurse into the member.
                        out.extend(nested);
                    }
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
        None
    }

    #[test]
    fn splice_preserves_prose_and_is_idempotent() {
        let doc = "intro prose\n\n## Tools\n\n- `stale_tool` — old\n\n## Middle\n\nhand-written\n\n## Resources (tui://)\n\n- `tui://gone` — old\n";
        let spliced = splice_skill_sections(doc).expect("both headings present");
        assert!(spliced.contains("intro prose"));
        assert!(spliced.contains("## Middle\n\nhand-written"));
        assert!(!spliced.contains("stale_tool"), "old tools replaced");
        assert!(spliced.contains(&skill_tool_section()));
        assert!(spliced.contains(&skill_resource_section()));
        // Splicing an already-current doc is a no-op.
        assert_eq!(
            splice_skill_sections(&spliced).as_deref(),
            Some(spliced.as_str())
        );
    }

    #[test]
    fn splice_rejects_missing_headings() {
        assert!(splice_skill_sections("no headings here").is_none());
        assert!(splice_skill_sections("## Tools\n\nok only").is_none());
    }

    #[test]
    fn skill_doc_ships_the_generated_sections() {
        // The compiled-in skill document must be regenerated from the
        // registry, not hand-edited: its Tools and Resources sections are
        // byte-equal to what the generators produce right now. A failure
        // here means the registry changed without rerunning
        // `cargo run -- skill` (or editing SKILL.md by hand).
        let doc = include_str!("../../SKILL.md");
        assert!(
            doc.contains(&skill_tool_section()),
            "SKILL.md's Tools section drifted from the registry — regenerate: cargo run -- skill"
        );
        assert!(
            doc.contains(&skill_resource_section()),
            "SKILL.md's Resources section drifted from the registry — regenerate: cargo run -- skill"
        );
        for r in RESOURCES {
            assert!(doc.contains(r.uri), "SKILL.md misses resource {}", r.uri);
        }
    }

    /// Audit finding 32: permanently unsupported operations must be named
    /// as such IN THE SURFACE an agent reads before calling — not
    /// discoverable only by invoking the tool and eating an error. Each
    /// (tool, selector, keyword) triple here asserts the tool description
    /// (the machine-readable contract on the wire) marks the member
    /// explicitly unsupported with the reason.
    #[test]
    fn permanently_unsupported_selectors_are_named_in_descriptions() {
        let router = crate::mcp::TuiLabServer::tool_router();
        let desc = |tool: &str| {
            router
                .get(tool)
                .map(|t| t.description.clone().unwrap_or_default().to_string())
                .unwrap_or_else(|| panic!("tool {tool} missing from router"))
        };
        let coverage = desc("tui_coverage");
        // P2-58: the safest contract is that `uncovered` is not a selector
        // at all. The description must explain the missing denominator and
        // point to delta.
        assert!(
            coverage.to_lowercase().contains("no uncovered action")
                && coverage.to_lowercase().contains("denominator")
                && coverage.to_lowercase().contains("use delta"),
            "tui_coverage must explain that uncovered is absent because there is no denominator: {coverage}"
        );
        let snapshot = desc("tui_coverage");
        assert!(
            snapshot.contains("snapshot") && snapshot.contains("tuicov"),
            "tui_coverage description must state snapshot's tuicov requirement: {snapshot}"
        );
    }

    /// P1.9: the README's generated selector block must be byte-equal to
    /// what the generator produces right now — the README's public
    /// selector tables are DERIVED (one authoritative declaration, every
    /// doc derived), and a failure here means the registry changed
    /// without `cargo run -- skill --write-readme`.
    #[test]
    fn readme_selector_block_matches_registry() {
        let readme =
            match std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")) {
                Ok(r) => r,
                // README absent in an exotic checkout: nothing to keep in parity.
                Err(_) => return,
            };
        let section = readme_selector_section();
        assert!(
            readme.contains(&section),
            "README's generated selector block drifted from the registry — regenerate: cargo run -- skill --write-readme"
        );
        // And no hand-written selector line may resurrect outside the
        // generated block (that is the P1.9 drift class): the old
        // `**Actions:**`-style lines now live only in the generated span.
        let begin = readme
            .find("<!-- BEGIN GENERATED SELECTORS")
            .expect("begin marker");
        let end = readme
            .find("<!-- END GENERATED SELECTORS -->")
            .expect("end marker");
        for line in readme[..begin].lines().chain(readme[end..].lines()) {
            for prefix in [
                "**Actions:**",
                "**Modes:**",
                "**Conditions:**",
                "**Assertions:**",
                "**Profiles:**",
            ] {
                assert!(
                    !line.trim_start().starts_with(prefix),
                    "hand-written selector line outside the generated block: '{line}' — delete it, the registry generates these"
                );
            }
        }
    }

    /// Audit P1.8: every flow step must round-trip — the arguments were
    /// built from the real params types, so each must deserialize back
    /// into THAT tool's params type, and the tool name must exist in the
    /// registry. A flow that advertises a grammar the server rejects is a
    /// bug caught at build time, not agent runtime.
    #[test]
    fn flow_steps_roundtrip_into_real_param_types() {
        let flows = flows();
        let registry_names: std::collections::BTreeSet<&str> =
            TOOLS.iter().map(|t| t.name).collect();
        // (tool -> params-parse closure): a tiny typed dispatch over the
        // tools the flows reference. A flow step naming a tool not listed
        // here fails the test until it is added — deliberate friction.
        let parse: fn(&str, &serde_json::Value) -> Result<(), String> = |tool, args| {
            let v = args.clone();
            let parsed: Result<(), serde_json::Error> = match tool {
                "tui_session" => {
                    serde_json::from_value::<crate::mcp::params::TuiSessionParams>(v).map(|_| ())
                }
                "tui_observe" => {
                    serde_json::from_value::<crate::mcp::params::TuiObserveParams>(v).map(|_| ())
                }
                "tui_audit" => {
                    serde_json::from_value::<crate::mcp::params::TuiAuditParams>(v).map(|_| ())
                }
                "tui_workflow" => {
                    serde_json::from_value::<crate::mcp::params::TuiWorkflowParams>(v).map(|_| ())
                }
                "tui_probe" => {
                    serde_json::from_value::<crate::mcp::params::TuiProbeParams>(v).map(|_| ())
                }
                "tui_intent" => {
                    serde_json::from_value::<crate::mcp::params::TuiIntentParams>(v).map(|_| ())
                }
                "tui_framework" => {
                    serde_json::from_value::<crate::mcp::params::TuiFrameworkParams>(v).map(|_| ())
                }
                "tui_contract" => {
                    serde_json::from_value::<crate::mcp::params::TuiContractParams>(v).map(|_| ())
                }
                "tui_scenario" => {
                    serde_json::from_value::<crate::mcp::params::TuiScenarioParams>(v).map(|_| ())
                }
                "tui_run" => {
                    serde_json::from_value::<crate::mcp::params::TuiRunParams>(v).map(|_| ())
                }
                _ => {
                    return Err(format!(
                    "flow references tool '{tool}' with no typed parser — add one or fix the flow"
                ))
                }
            };
            parsed.map_err(|e| format!("{tool}: {e}"))
        };
        for (name, flow) in flows.as_object().expect("flows object") {
            if name == "note" {
                continue;
            }
            let steps = flow["steps"]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: steps array"));
            assert!(!steps.is_empty(), "{name}: no steps");
            for (i, s) in steps.iter().enumerate() {
                let tool = s["tool"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}[{i}]: tool name"));
                assert!(
                    registry_names.contains(tool),
                    "{name}[{i}]: tool '{tool}' not in the registry"
                );
                let args = &s["arguments"];
                assert!(args.is_object(), "{name}[{i}]: arguments must be an object");
                if let Err(e) = parse(tool, args) {
                    panic!("{name}[{i}]: flow arguments do not deserialize into the real params type: {e}\nargs: {args}");
                }
                assert!(
                    s["why"].as_str().is_some_and(|w| !w.is_empty()),
                    "{name}[{i}]: every step carries a why"
                );
            }
        }
        // The audit's named flows all exist.
        for required in [
            "debug_existing_tui",
            "construct_or_refine_tui",
            "regression_test_tui",
        ] {
            assert!(
                flows.get(required).is_some(),
                "required flow '{required}' missing"
            );
        }
    }
}
