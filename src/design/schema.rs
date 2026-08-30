use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignContract {
    pub schema: ContractSchema,
    pub viewports: Vec<ViewportReq>,
    pub keybindings: Vec<Keybinding>,
    pub escape_closes_modal: bool,
    pub reverse_tab_required: bool,
    pub destructive_require_confirmation: bool,
    pub volatile_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractSchema {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewportReq {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    pub action: String,
    pub keys: Vec<String>,
}
