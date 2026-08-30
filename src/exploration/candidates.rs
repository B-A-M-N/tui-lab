//! Novel-action candidates for Hermes-guided exploration (spec section 4.3).
//! Returns compact suggestions; Hermes chooses. No hidden LLM.

use crate::screen::ScreenState;
use crate::semantic::SemanticScreen;
use serde_json::json;

pub fn suggest(_screen: &ScreenState, sem: &SemanticScreen) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    // If a dialog/region exists but focus is unknown, suggest Tab traversal.
    if sem.focus.control.is_none() {
        out.push(json!({
            "action": "key",
            "key": "tab",
            "reason": "focus traversal not yet established from this state"
        }));
    }
    // If there is a focused button, suggest activating it.
    if let Some(f) = &sem.focus.control {
        out.push(json!({
            "action": "key",
            "key": "enter",
            "reason": format!("focused control '{}' not yet activated", f)
        }));
    }
    // Always offer a baseline resize to anchor layout tests.
    out.push(json!({
        "action": "resize",
        "cols": 80,
        "rows": 24,
        "reason": "layout not yet tested at baseline viewport"
    }));
    out
}
