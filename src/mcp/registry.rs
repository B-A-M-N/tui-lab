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
        summary: "Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state, protocol trace + mode timeline (portable-pty/line engines), pipe stdout/stderr streams.",
        selector: Some(("mode", <crate::mcp::params::ObserveMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_act",
        summary: "Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema). Optional `completion` declares how \"done\" means (stable_screen/first_change/any_change/text_appears/text_disappears/process_exit/command_done/bell/semantic_change/may_be_silent/no_wait) so a silent/exit action is never misreported as settled=false.",
        selector: None,
    },
    ToolCapability {
        name: "tui_wait",
        summary: "Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.",
        selector: Some(("condition", <crate::mcp::params::WaitCondition as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_probe",
        summary: "Run one small experiment and get EVERYTHING materially different: baseline vs settled after-frame, causal events inside the probe window, transition, watched anomalies. stimulus {kind:none} = drift probe.",
        selector: Some(("completion", <crate::mcp::params::ProbeCompletion as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_assert",
        summary: "Assert UI facts; unknown assertions are invalid_request (caller error), never assertion_failed (UI failure). `oracle` evaluates the shared Wave E language.",
        selector: Some(("assertion", <crate::mcp::params::AssertAssertion as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_checkpoint",
        summary: "Save and compare named UI state checkpoints (durable under persistent runs).",
        selector: Some(("action", <crate::mcp::params::CheckpointAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_scenario",
        summary: "Record, save, list, export, and replay interaction scenarios (session+generation scoped).",
        selector: Some(("action", <crate::mcp::params::ScenarioAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_record",
        summary: "Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures.",
        selector: Some(("format", <crate::mcp::params::RecordFormat as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_explore",
        summary: "Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, and the state graph. Driving: blocked while a human lease is live.",
        selector: Some(("mode", <crate::mcp::params::ExploreMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_audit",
        summary: "Deterministic UX audits returning evidence-backed findings; `full` is the composite. label=/compare_to= diff findings across runs. Safe-only default: invasive profiles are withheld (ORCH-GATED) until allow_mutation=true; deep_isolation=true restart-replays between mutating drivers. Active profiles are blocked while a human lease is live.",
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
        summary: "Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, repair packets for every finding, repair bundle for ONE finding (reproduction + app-attested source loci + targeted verification recipe + regression check), and context (this registry as JSON).",
        selector: Some(("action", <crate::mcp::params::RunAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_contract",
        summary: "Design contracts: load, validate, conformance status, baseline compare (regressions become findings), and scaffold — generate a starter contract from the LIVE observed frame (regions become components, named controls become oracle assertions; carries the scaffold.inferred marker; edit from observation toward intent).",
        selector: Some(("action", <crate::mcp::params::ContractAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_explain",
        // `finding_id` is an open set (any recorded finding), not a closed
        // selector — like `tui_act`.
        summary: "Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities (via the live profile) the finding is conditional on.",
        selector: None,
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
        uri: "tui://sessions/{session_id}/semantic",
        description: "Live semantic screen: regions, controls, focus, affordances, components.",
    },
    ResourceCapability {
        uri: "tui://sessions/{session_id}/screen",
        description: "Live screen text + geometry.",
    },
    ResourceCapability {
        uri: "tui://findings",
        description: "Findings accumulated this run (audits, contracts, exploration).",
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
}
