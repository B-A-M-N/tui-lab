//! Audit-profile parity: the engine's descriptor table and the MCP
//! selector enum must agree (review §7). The `LifecycleExit` drift — a
//! profile the engine parsed but MCP could not select — is the regression
//! this suite pins shut. If either side gains a profile without the
//! other, these tests fail with a diff, not a silent weaker audit.

use std::str::FromStr;
use tui_lab::mcp::params::{AuditProfile as McpAuditProfile, EnumVariants};

#[test]
fn engine_table_and_mcp_enum_name_the_same_profiles() {
    let engine: Vec<&str> = tui_lab::audit::orchestrator::PROFILES
        .iter()
        .map(|d| d.id)
        .collect();
    let mut wire: Vec<&str> = McpAuditProfile::VARIANTS.to_vec();
    let mut e2 = engine.clone();
    e2.sort();
    wire.sort();
    assert_eq!(
        e2, wire,
        "engine descriptors and MCP selector enum have drifted — add the profile to BOTH (engine: src/audit/orchestrator.rs PROFILES; wire: src/mcp/params.rs selector_enum! AuditProfile)"
    );
}

#[test]
fn engine_names_map_to_parseable_mcp_selectors() {
    for d in tui_lab::audit::orchestrator::PROFILES {
        let mcp = McpAuditProfile::from_str(d.id)
            .unwrap_or_else(|e| panic!("MCP enum must accept engine profile '{}': {e}", d.id));
        assert_eq!(mcp.as_str(), d.id, "wire name round-trip for {}", d.id);
    }
}

#[test]
fn process_consuming_profiles_are_never_in_full() {
    for d in tui_lab::audit::orchestrator::PROFILES {
        if d.risk == tui_lab::audit::orchestrator::MutationRisk::RestartRequired {
            assert!(
                !d.included_in_full,
                "{} consumes the process and must never be a full member",
                d.id
            );
        }
    }
}
