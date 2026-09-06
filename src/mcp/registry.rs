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
        assert!(
            coverage.contains("uncovered")
                && (coverage.contains("explicitly unsupported")
                    || coverage.contains("unsupported")),
            "tui_coverage description must mark 'uncovered' unsupported up front: {coverage}"
        );
        let snapshot = desc("tui_coverage");
        assert!(
            snapshot.contains("snapshot") && snapshot.contains("tuicov"),
            "tui_coverage description must state snapshot's tuicov requirement: {snapshot}"
        );
    }
}
