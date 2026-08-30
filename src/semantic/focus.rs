//! Focus inference (spec section 10). Track focus through evidence: reverse-video
//! transitions, cursor position, known control bounds.

use crate::screen::ScreenState;
use crate::semantic::controls::{Control, ControlKind};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusInfo {
    /// Label of the control believed to hold focus, if any.
    pub control: Option<String>,
    /// Stable ID of the focused control, when the label matched a known
    /// [`Control`]. Prefer this over `control` for cross-frame tracking.
    #[serde(default)]
    pub control_id: Option<String>,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Temporal focus tracker: feeds successive [`FocusInfo`] frames and reports
/// focus *switches* (audit item 29 — tab-order and focus audits need the
/// transition, not just the current holder).
#[derive(Debug, Clone, Default)]
pub struct FocusTracker {
    last_control: Option<String>,
    switches: u32,
}

impl FocusTracker {
    pub fn new() -> Self {
        FocusTracker::default()
    }

    /// Feed one frame. Returns `Some(new_label)` when focus moved to a
    /// *different* control; `None` when it stayed put or no control holds
    /// focus. An absent focus never resets the last known holder.
    pub fn update(&mut self, info: &FocusInfo) -> Option<String> {
        match &info.control {
            Some(label) => {
                if self.last_control.as_deref() != Some(label.as_str()) {
                    self.last_control = Some(label.clone());
                    self.switches += 1;
                    Some(label.clone())
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Number of focus switches observed so far (first focus counts).
    pub fn switches(&self) -> u32 {
        self.switches
    }

    /// The most recent control known to hold focus, if any.
    pub fn last(&self) -> Option<&str> {
        self.last_control.as_deref()
    }
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
                        control_id: Some(c.id.clone()),
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
            control_id: Some(c.id.clone()),
            confidence: 0.8,
            evidence: vec![format!("cursor-at {},{}", cx, cy)],
        };
    }
    // 3) cursor only
    FocusInfo {
        control: None,
        control_id: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(label: Option<&str>) -> FocusInfo {
        FocusInfo {
            control: label.map(|s| s.to_string()),
            control_id: None,
            confidence: 0.9,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn tracker_same_control_twice_is_not_a_switch() {
        let mut t = FocusTracker::new();
        assert_eq!(t.update(&info(Some("Save"))), Some("Save".to_string()));
        assert_eq!(t.update(&info(Some("Save"))), None);
        assert_eq!(t.switches(), 1);
        assert_eq!(t.last(), Some("Save"));
    }

    #[test]
    fn tracker_change_reports_switch_and_counts() {
        let mut t = FocusTracker::new();
        t.update(&info(Some("Save")));
        assert_eq!(t.update(&info(Some("Cancel"))), Some("Cancel".to_string()));
        assert_eq!(t.switches(), 2);
        assert_eq!(t.last(), Some("Cancel"));
    }

    #[test]
    fn tracker_absent_focus_keeps_last_holder() {
        let mut t = FocusTracker::new();
        t.update(&info(Some("Save")));
        assert_eq!(t.update(&info(None)), None);
        assert_eq!(t.switches(), 1, "absent focus is not a switch");
        assert_eq!(t.last(), Some("Save"), "absent focus must not reset last");
    }

    #[test]
    fn infer_focus_populates_control_id() {
        use crate::screen::{Cell, Color, CursorState, ProcessState, ScreenState};
        use crate::semantic::controls::{Control, ControlBounds};
        let mut screen = ScreenState {
            cols: 20,
            rows: 3,
            cursor: CursorState { x: 0, y: 1, visible: true },
            title: None,
            cells: vec![Cell {
                x: 0,
                y: 1,
                text: "S".into(),
                fg: Color::unknown(),
                bg: Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: true,
                strike: false,
            }],
            viewport_text: vec![
                "".to_string(),
                "Save".to_string(),
                "".to_string(),
            ],
            scrollback: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        };
        let ctl = Control {
            id: "button:save:0,1".into(),
            kind: ControlKind::Button,
            label: "Save".into(),
            value: None,
            bounds: ControlBounds { x: 0, y: 1, width: 4, height: 1 },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
            evidence: Vec::new(),
            source: "inferred".into(),
        };
        let info = infer_focus(&screen, &[ctl]);
        assert_eq!(info.control.as_deref(), Some("Save"));
        assert_eq!(info.control_id.as_deref(), Some("button:save:0,1"));
        let _ = &mut screen; // keep mutable binding honest if assertions evolve
    }
}
