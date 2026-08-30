//! UX audit engine: deterministic rules + evidence (spec section 14-21).
//! The LLM (Hermes) may interpret results afterward; the engine only emits
//! findings with evidence.

use crate::screen::ScreenState;
use crate::semantic::{Control, ControlKind, SemanticScreen};
use serde_json::json;

pub mod driver;
pub use driver::*;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub id: String,
    pub severity: String, // error | warn | info
    pub category: String,
    pub summary: String,
    pub evidence: serde_json::Value,
    pub confidence: f32,
}

/// Run the requested audit profile and return evidence-backed findings.
pub fn run(profile: &str, screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let want = |p: &str| profile == "full" || profile == p;
    if want("focus") {
        out.extend(static_focus_audit(screen, sem));
    }
    if want("layout") || want("clipping") {
        out.extend(static_clipping_audit(screen, sem));
    }
    if want("discoverability") {
        out.extend(discoverability_audit(screen));
    }
    if want("keyboard") {
        out.extend(static_keyboard_audit(sem));
    }
    if want("navigation") {
        out.push(Finding {
            id: "NAV-INFO".into(),
            severity: "info".into(),
            category: "navigation".into(),
            summary: "Navigation graph is built incrementally by tui_explore; static graph not available from a single frame.".into(),
            evidence: json!({}),
            confidence: 1.0,
        });
    }
    if want("color") || want("performance") || want("mouse") || want("states") || want("errors") {
        out.push(Finding {
            id: format!("{}-INFO", profile.to_uppercase()),
            severity: "info".into(),
            category: profile.into(),
            summary: format!("Profile '{}' static checks not yet implemented; structure available via tui_observe mode=semantic.", profile),
            evidence: json!({}),
            confidence: 1.0,
        });
    }
    out
}

fn static_focus_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let reverse_count = screen.cells.iter().filter(|c| c.reverse).count();
    if sem.focus.control.is_none() && reverse_count == 0 {
        out.push(Finding {
            id: "FOCUS-001".into(),
            severity: "warn".into(),
            category: "focus".into(),
            summary: "No detectable focus target on this screen.".into(),
            evidence: json!({ "reverse_cells": reverse_count, "cursor": screen.cursor }),
            confidence: 0.7,
        });
    } else if sem.focus.control.is_some() {
        out.push(Finding {
            id: "FOCUS-OK".into(),
            severity: "info".into(),
            category: "focus".into(),
            summary: format!(
                "Focus on '{}' (confidence {:.2})",
                sem.focus.control.as_deref().unwrap_or(""),
                sem.focus.confidence
            ),
            evidence: json!({ "control": sem.focus.control, "evidence": sem.focus.evidence }),
            confidence: sem.focus.confidence,
        });
    }
    out
}

fn static_clipping_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    for rg in &sem.regions {
        let b = &rg.bounds;
        if (b.x + b.width) > screen.cols || (b.y + b.height) > screen.rows {
            out.push(Finding {
                id: "CLIP-001".into(),
                severity: "error".into(),
                category: "clipping".into(),
                summary: format!("Region '{}' extends beyond terminal bounds.", rg.id),
                evidence: json!({ "bounds": b, "terminal": { "cols": screen.cols, "rows": screen.rows } }),
                confidence: 0.96,
            });
        }
    }
    out
}

fn discoverability_audit(screen: &ScreenState) -> Vec<Finding> {
    let joined = screen.viewport_text.join("\n").to_lowercase();
    let has_hints = joined.contains("help")
        || joined.contains("press")
        || joined.contains("ctrl+")
        || joined.contains("enter to");
    let mut out = Vec::new();
    if !has_hints {
        out.push(Finding {
            id: "DISC-001".into(),
            severity: "warn".into(),
            category: "discoverability".into(),
            summary: "No visible keyboard/help hints detected on this screen.".into(),
            evidence: json!({ "note": "some TUIs hide hints intentionally; confirm against design contract" }),
            confidence: 0.5,
        });
    } else {
        out.push(Finding {
            id: "DISC-OK".into(),
            severity: "info".into(),
            category: "discoverability".into(),
            summary: "Visible discoverability hints present.".into(),
            evidence: json!({}),
            confidence: 1.0,
        });
    }
    out
}

fn static_keyboard_audit(sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let buttons: Vec<&Control> = sem
        .controls
        .iter()
        .filter(|c| c.kind == ControlKind::Button)
        .collect();
    if !buttons.is_empty() {
        out.push(Finding {
            id: "KB-INFO".into(),
            severity: "info".into(),
            category: "keyboard".into(),
            summary: format!("Detected {} button-like control(s); verify Tab/Enter reachability via tui_explore.", buttons.len()),
            evidence: json!({ "controls": buttons.iter().map(|b| &b.label).collect::<Vec<_>>() }),
            confidence: 0.85,
        });
    }
    out
}
