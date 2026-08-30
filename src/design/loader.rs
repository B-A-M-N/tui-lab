use super::schema::DesignContract;
use std::path::Path;

/// Load a design contract from a YAML file.
pub fn load_design_contract(path: &Path) -> Result<DesignContract, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read design contract: {}", e))?;

    // Try YAML first, then JSON
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse design contract JSON: {}", e))
    } else {
        // For now, return a default contract if YAML parsing isn't available
        // TODO: Add serde_yaml dependency when needed
        Err(
            "YAML parsing not yet implemented. Use JSON format or implement with serde_yaml."
                .to_string(),
        )
    }
}

/// Create a default design contract.
pub fn default_contract() -> DesignContract {
    DesignContract {
        schema: super::schema::ContractSchema {
            name: "default".to_string(),
            version: "1".to_string(),
        },
        viewports: vec![
            super::schema::ViewportReq { cols: 80, rows: 24 },
            super::schema::ViewportReq {
                cols: 100,
                rows: 30,
            },
            super::schema::ViewportReq {
                cols: 120,
                rows: 40,
            },
        ],
        keybindings: vec![],
        escape_closes_modal: true,
        reverse_tab_required: true,
        destructive_require_confirmation: true,
        volatile_patterns: vec![r"\bCPU \d+%".to_string(), r"\b\d\d:\d\d:\d\d\b".to_string()],
    }
}
