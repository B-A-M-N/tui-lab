//! Contract loading (Wave E items 39, 44): real YAML + JSON parsing.
//!
//! YAML is the authoring format (`.yaml` / `.yml`); JSON also loads (older
//! contract files, programmatic generation). Parsing is strict:
//! `deny_unknown_fields` on the schema means a typo like
//! `escap_closes_modal` fails the load with the offending field named, and
//! oracle expressions are validated at load time via
//! [`ProjectContract::validate`].

use super::schema::ProjectContract;
use std::path::Path;

/// Load a project contract from a YAML or JSON file.
pub fn load_design_contract(path: &Path) -> Result<ProjectContract, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read design contract: {}", e))?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "json" => parse_json(&content),
        "yaml" | "yml" => parse_yaml(&content),
        "" => {
            // Extension-less: sniff. JSON documents start with `{`.
            if content.trim_start().starts_with('{') {
                parse_json(&content)
            } else {
                parse_yaml(&content)
            }
        }
        other => Err(format!(
            "unsupported contract extension '.{other}' (expected .yaml, .yml, or .json)"
        )),
    }
}

/// Parse a contract from YAML text.
pub fn parse_yaml(content: &str) -> Result<ProjectContract, String> {
    let c: ProjectContract = serde_yaml::from_str(content)
        .map_err(|e| format!("Failed to parse design contract YAML: {}", e))?;
    validate_loaded(&c)?;
    Ok(c)
}

/// Parse a contract from JSON text.
pub fn parse_json(content: &str) -> Result<ProjectContract, String> {
    let c: ProjectContract = serde_json::from_str(content)
        .map_err(|e| format!("Failed to parse design contract JSON: {}", e))?;
    validate_loaded(&c)?;
    Ok(c)
}

/// Post-parse validation: unknown predicates / bad regexes / duplicate names
/// become load errors, not silent no-ops. The first problem is returned
/// (the full list is available through `ProjectContract::validate()` for
/// reporting surfaces like `tui_contract action=validate`).
fn validate_loaded(c: &ProjectContract) -> Result<(), String> {
    let problems = c.validate();
    let failures: Vec<_> = problems
        .iter()
        .filter(|r| r.verdict == super::conformance::Verdict::Fail)
        .collect();
    if failures.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = failures
        .iter()
        .map(|r| format!("  [{}] {}: {}", r.verdict.as_str(), r.name, r.detail))
        .collect();
    Err(format!(
        "contract '{}' has {} document problem(s):\n{}",
        c.schema.name,
        failures.len(),
        details.join("\n")
    ))
}

/// Serialize a contract back to YAML (for `tui_contract action=export` and
/// template generation).
pub fn to_yaml(c: &ProjectContract) -> Result<String, String> {
    serde_yaml::to_string(c).map_err(|e| format!("Failed to serialize contract YAML: {}", e))
}

/// Create a default design contract (legacy helper: the old hardcoded
/// defaults, now expressed as a real document).
pub fn default_contract() -> ProjectContract {
    ProjectContract::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_YAML: &str = r##"
schema:
  name: connections-tui
  version: "2"
launch:
  command: python3
  args: ["-m", "connections"]
  cols: 100
  rows: 30
viewports:
  - cols: 80
    rows: 24
  - cols: 120
    rows: 40
escape_closes_modal: true
reverse_tab_required: true
volatile_patterns:
  - "\\bCPU \\d+%"
components:
  - name: main-table
    role: table
    required: true
    expect:
      - "no_clipping()"
interactions:
  - name: open-add-dialog
    keys: ["n"]
    context: from the connections list
    expect:
      - "modal_open()"
      - "focused(\"#button/save\")"
layout:
  - name: narrow-survival
    min_cols: 60
    min_rows: 20
oracles:
  - id: host-field-present
    expr: "visible(\"Host\")"
"##;

    #[test]
    fn parses_good_yaml() {
        let c = parse_yaml(GOOD_YAML).expect("parses");
        assert_eq!(c.schema.name, "connections-tui");
        assert_eq!(c.launch.as_ref().unwrap().cols, 100);
        assert_eq!(c.components.len(), 1);
        assert_eq!(c.components[0].expect.len(), 1);
        assert_eq!(c.interactions[0].keys, vec!["n".to_string()]);
        assert_eq!(c.interactions[0].expect.len(), 2);
        assert_eq!(c.layout[0].min_cols, Some(60));
        assert_eq!(c.oracles[0].id.as_deref(), Some("host-field-present"));
        assert_eq!(c.volatile_patterns.len(), 1);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let bad = GOOD_YAML.replace("escape_closes_modal", "escap_closes_modal");
        let err = parse_yaml(&bad).unwrap_err();
        assert!(
            err.contains("unknown field") || err.to_lowercase().contains("unknown"),
            "typo'd field must fail the load: {err}"
        );
    }

    #[test]
    fn bad_oracle_fails_load() {
        let bad = GOOD_YAML.replace("no_clipping()", "frobnicate_everything()");
        let err = parse_yaml(&bad).unwrap_err();
        assert!(
            err.contains("unknown oracle predicate"),
            "oracle typo must fail the load: {err}"
        );
    }

    #[test]
    fn bad_regex_fails_load() {
        // The YAML text carries the double backslash literally
        // (`"\\bCPU \\d+%"` in the raw string → `\bCPU \d+%` on the wire).
        let bad = GOOD_YAML.replace("\\\\bCPU \\\\d+%", "[invalid");
        assert_ne!(bad, GOOD_YAML, "replace must hit");
        let err = parse_yaml(&bad).unwrap_err();
        assert!(
            err.to_lowercase().contains("invalid regex"),
            "bad regex must fail the load: {err}"
        );
    }

    #[test]
    fn json_also_loads() {
        let c = default_contract();
        let json = serde_json::to_string_pretty(&c).unwrap();
        let parsed = parse_json(&json).expect("round-trips");
        assert_eq!(parsed.viewports.len(), 3);
    }

    #[test]
    fn yaml_round_trip() {
        let c = parse_yaml(GOOD_YAML).unwrap();
        let text = to_yaml(&c).unwrap();
        let c2 = parse_yaml(&text).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn duplicate_names_fail_load() {
        // Add a second component entry that reuses the name "main-table".
        let bad = GOOD_YAML.replace(
            "components:\n  - name: main-table\n    role: table\n    required: true\n    expect:\n      - \"no_clipping()\"",
            "components:\n  - name: main-table\n    role: table\n    required: true\n    expect:\n      - \"no_clipping()\"\n  - name: main-table\n    role: tree",
        );
        assert_ne!(bad, GOOD_YAML, "replace must hit");
        let err = parse_yaml(&bad).unwrap_err();
        assert!(
            err.contains("duplicate component name"),
            "duplicates must fail: {err}"
        );
    }

    // ── Wave 5: contracts without rigidity (items 33-36) ──

    #[test]
    fn extensions_namespace_round_trips_and_defaults_empty() {
        // A contract with adapter-specific extension keys loads fine —
        // `extensions:` is the sanctioned place for them (item 34).
        let yaml = GOOD_YAML.replace(
            "schema:\n  name: connections-tui\n  version: \"2\"",
            "schema:\n  name: connections-tui\n  version: \"2\"\n  mode: strict\n  extensions:\n    ratatui.weight_min: 12\n    custom.vendor_note: \"hello\"",
        );
        let c = parse_yaml(&yaml).expect("extensions load");
        assert_eq!(c.schema.mode, crate::design::ContractMode::Strict);
        assert_eq!(c.schema.extensions.len(), 2);
        assert_eq!(
            c.schema.extensions.get("ratatui.weight_min"),
            Some(&serde_json::json!(12))
        );
        // Round-trips through YAML.
        let text = to_yaml(&c).unwrap();
        let c2 = parse_yaml(&text).expect("round-trip");
        assert_eq!(c, c2);
        // A contract without extensions carries the empty map (not an
        // error), and the default mode is Advisory.
        let plain = parse_yaml(GOOD_YAML).unwrap();
        assert!(plain.schema.extensions.is_empty());
        assert_eq!(plain.schema.mode, crate::design::ContractMode::Advisory);
    }

    #[test]
    fn mode_parses_all_three() {
        for (text, want) in [
            ("advisory", crate::design::ContractMode::Advisory),
            ("validation", crate::design::ContractMode::Validation),
            ("strict", crate::design::ContractMode::Strict),
        ] {
            let yaml =
                GOOD_YAML.replace("version: \"2\"", &format!("version: \"2\"\n  mode: {text}"));
            let c = parse_yaml(&yaml).unwrap();
            assert_eq!(c.schema.mode, want, "mode={text}");
        }
    }

    #[test]
    fn unknown_mode_fails_load() {
        let yaml = GOOD_YAML.replace("version: \"2\"", "version: \"2\"\n  mode: pedantic");
        let err = parse_yaml(&yaml).unwrap_err();
        assert!(
            err.to_lowercase().contains("mode") || err.to_lowercase().contains("unknown"),
            "unknown mode must fail the load naming the field: {err}"
        );
    }
}
