//! InvokeAffordance — request a named custom-widget affordance without adding
//! a core verb (W2.9, review: affordances are per-screen, verified, not
//! assumed). A thin request record that resolves **honestly** against the
//! affordances actually inferred on the current screen — it never claims a
//! target affordance exists before checking the screen's own inference.

use crate::semantic::affordance::Affordance;

/// A request to fire a named affordance on a target control or widget.
///
/// `target` is a stable control id or a widget id as currently on screen;
/// `affordance_id` names the action (control shortcut, "quit", "save", …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InvokeAffordance {
    pub target: String,
    pub affordance_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AffordanceResolution {
    /// The named affordance is available on the target right now.
    Available,
    /// Not on the target (but exists on the screen somewhere).
    NotOnTarget,
    /// No matching affordance anywhere on the current screen.
    Missing,
}

impl InvokeAffordance {
    /// Check this request against the affordances inferred on the current
    /// screen. Honest: returns [`AffordanceResolution::Available`] only when
    /// a real inferred affordance matches both target and id.
    pub fn resolve(&self, screen_affordances: &[Affordance]) -> AffordanceResolution {
        let on_target: Vec<&Affordance> = screen_affordances
            .iter()
            .filter(|a| a.control_id.as_deref() == Some(self.target.as_str()))
            .collect();
        if on_target.iter().any(|a| a.action == self.affordance_id) {
            return AffordanceResolution::Available;
        }
        if screen_affordances.iter().any(|a| a.action == self.affordance_id) {
            return AffordanceResolution::NotOnTarget;
        }
        AffordanceResolution::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::affordance::{Invocation, Visibility};
    use crate::semantic::confidence::Confidence;

    fn aff(action: &str, control_id: Option<&str>) -> Affordance {
        Affordance {
            action: action.to_string(),
            control_id: control_id.map(String::from),
            invocation: Invocation::Activate,
            visibility: Visibility::Labeled,
            hint_text: None,
            confidence: Confidence::inferred(0.9, &["test"]),
            source: "test".into(),
        }
    }

    #[test]
    fn resolves_only_when_target_and_id_match() {
        let req = InvokeAffordance {
            target: "#save".into(),
            affordance_id: "save".into(),
        };
        // Present on the exact target.
        let res = req.resolve(&[aff("save", Some("#save"))]);
        assert!(matches!(res, AffordanceResolution::Available));
        // Id exists elsewhere but not on this target.
        let res = req.resolve(&[aff("save", Some("#other"))]);
        assert!(matches!(res, AffordanceResolution::NotOnTarget));
        // Nothing at all on screen.
        let res = req.resolve(&[aff("quit", Some("#q"))]);
        assert!(matches!(res, AffordanceResolution::Missing));
    }
}