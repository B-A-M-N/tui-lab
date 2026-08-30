//! Focus inference (spec section 10). Track focus through evidence: reverse-video
//! transitions, cursor position, known control bounds.

use crate::screen::ScreenState;
use crate::semantic::controls::{Control, ControlKind};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusInfo {
    /// Label of the control believed to hold focus, if any.
    pub control: Option<String>,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Infer the focused control: prefer a control whose cell is reverse-video or
/// contains the cursor; otherwise the cursor position alone.
pub fn infer_focus(screen: &ScreenState, controls: &[Control]) -> FocusInfo {
    // 1) reverse-video control
    for (idx, cell) in screen.cells.iter().enumerate() {
        if cell.reverse {
            let (x, y) = (cell.x, cell.y);
            if let Some(c) = control_at(controls, x, y) {
                if c.kind == ControlKind::Button || c.kind == ControlKind::Field {
                    return FocusInfo {
                        control: Some(c.label.clone()),
                        confidence: 0.94,
                        evidence: vec!["reverse-video".into(), format!("at {},{}", x, y)],
                    };
                }
                let _ = idx;
            }
        }
    }
    // 2) cursor on a control
    let (cx, cy) = (screen.cursor.x, screen.cursor.y);
    if let Some(c) = control_at(controls, cx, cy) {
        return FocusInfo {
            control: Some(c.label.clone()),
            confidence: 0.8,
            evidence: vec![format!("cursor-at {},{}", cx, cy)],
        };
    }
    // 3) cursor only
    FocusInfo {
        control: None,
        confidence: if screen.cursor.visible { 0.3 } else { 0.1 },
        evidence: vec![format!("cursor-at {},{}", cx, cy)],
    }
}

fn control_at(controls: &[Control], x: u16, y: u16) -> Option<&Control> {
    // Controls are single-line at (bx, by). Treat the control as covering its
    // label start cell (good enough for focus heuristics at this stage).
    controls.iter().find(|c| {
        c.bounds.y == y && x >= c.bounds.x && x < c.bounds.x + c.bounds.width
    })
}
