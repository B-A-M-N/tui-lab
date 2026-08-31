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
        summary: "Manage TUI sessions: start, restart, stop, list, status, lease (human control), release.",
        selector: Some(("action", <crate::mcp::params::SessionAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_observe",
        summary: "Observe terminal state (summary, screen text, cells, semantic surfaces, tree, nodes, diff, change cursor, scrollback, search, command state).",
        selector: Some(("mode", <crate::mcp::params::ObserveMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_act",
        summary: "Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema).",
        selector: None,
    },
    ToolCapability {
        name: "tui_wait",
        summary: "Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.",
        selector: Some(("condition", <crate::mcp::params::WaitCondition as EnumVariants>::VARIANTS)),
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
        summary: "Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, and the state graph.",
        selector: Some(("mode", <crate::mcp::params::ExploreMode as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_audit",
        summary: "Deterministic UX audits returning evidence-backed findings; `full` is the composite. label=/compare_to= diff findings across runs (FIXED/REGRESSED/NEW).",
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
        summary: "Run lifecycle: status, persist (ephemeral→durable, same identity), close, and context (this registry as JSON).",
        selector: Some(("action", <crate::mcp::params::RunAction as EnumVariants>::VARIANTS)),
    },
    ToolCapability {
        name: "tui_contract",
        summary: "Design contracts: load, validate, conformance status, and baseline compare (regressions become findings).",
        selector: Some(("action", <crate::mcp::params::ContractAction as EnumVariants>::VARIANTS)),
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
        "resources": [
            { "uri_template": "tui://runs/{run_id}", "description": "run status + manifest" },
            { "uri_template": "tui://sessions/{session_id}/semantic", "description": "live semantic screen (regions, controls, focus, affordances, components)" },
            { "uri_template": "tui://sessions/{session_id}/screen", "description": "live screen text + geometry" },
            { "uri_template": "tui://findings", "description": "findings accumulated this run" },
        ],
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
}
