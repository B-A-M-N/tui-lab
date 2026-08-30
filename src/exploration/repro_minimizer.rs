//! Reproduction minimization via delta debugging (spec item 16).
//!
//! Given a long action trace that produces a crash or failure, finds the
//! minimal subsequence that still reproduces the issue.

use serde::{Deserialize, Serialize};

/// A single action in a reproduction trace.
///
/// Carries the full [`CanonicalAction`] (re-review: `{index, name}` could
/// count an action but never replay it — coordinates, typed text, and
/// modifiers were all lost). `index` preserves the position in the original
/// trace so minimized reports still point at real steps; `name` is derived
/// from the action via [`CanonicalAction::name`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReproAction {
    pub index: u32,
    pub action: crate::execution::CanonicalAction,
}

impl ReproAction {
    /// Display/evidence name, delegated to the canonical action.
    pub fn name(&self) -> &'static str {
        self.action.name()
    }
}

/// Result of a reproduction attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReproResult {
    pub actions_executed: u32,
    pub reproduced: bool,
    pub failure_type: Option<String>,
}

/// Minimizer: finds the smallest subsequence of actions that reproduces a failure.
pub struct ReproMinimizer<F>
where
    F: Fn(&[ReproAction]) -> ReproResult,
{
    test_fn: F,
}

impl<F> ReproMinimizer<F>
where
    F: Fn(&[ReproAction]) -> ReproResult,
{
    /// Create a new minimizer with a test function.
    pub fn new(test_fn: F) -> Self {
        ReproMinimizer { test_fn }
    }

    /// Minimize the action sequence using delta debugging.
    pub fn minimize(&self, actions: &[ReproAction]) -> Vec<ReproAction> {
        if actions.is_empty() {
            return Vec::new();
        }

        // Verify the full sequence reproduces the issue
        let full_result = (self.test_fn)(actions);
        if !full_result.reproduced {
            // Can't minimize what doesn't reproduce
            return actions.to_vec();
        }

        let mut current = actions.to_vec();
        let mut n = 2u32; // Initial granularity

        loop {
            if n as usize >= current.len() {
                break;
            }

            let chunk_size = (current.len() as f64 / n as f64).ceil() as usize;
            if chunk_size == 0 {
                break;
            }

            let mut reduced = false;

            // Try removing each chunk
            for i in 0..n as usize {
                let start = i * chunk_size;
                let end = ((i + 1) * chunk_size).min(current.len());
                if start >= current.len() {
                    break;
                }

                let candidate: Vec<ReproAction> = current[..start]
                    .iter()
                    .chain(&current[end..])
                    .cloned()
                    .collect();

                if candidate.is_empty() {
                    continue;
                }

                let result = (self.test_fn)(&candidate);
                if result.reproduced {
                    current = candidate;
                    n = (n - 1).max(2);
                    reduced = true;
                    break;
                }
            }

            if !reduced {
                // Increase granularity
                n = (n * 2).min(current.len() as u32);
                if n as usize == current.len() {
                    break;
                }
            }
        }

        // Final pass: try removing individual actions
        let mut final_actions = current.clone();
        let mut i = 0;
        while i < final_actions.len() {
            let candidate: Vec<ReproAction> = final_actions[..i]
                .iter()
                .chain(&final_actions[i + 1..])
                .cloned()
                .collect();

            if candidate.is_empty() {
                i += 1;
                continue;
            }

            let result = (self.test_fn)(&candidate);
            if result.reproduced {
                final_actions = candidate;
                // Don't increment i, we removed one
            } else {
                i += 1;
            }
        }

        final_actions
    }

    /// Generate a YAML representation of the minimal reproduction.
    pub fn to_yaml(&self, actions: &[ReproAction]) -> String {
        let mut yaml = String::from("schema: tui-lab/repro/v1\n");
        yaml.push_str(&format!("actions: {}\n", actions.len()));
        yaml.push_str("steps:\n");
        for ra in actions {
            yaml.push_str(&format!("  - action: {}\n", ra.name()));
            yaml.push_str(&format!("    index: {}\n", ra.index));
            // The replayable payload: the canonical action's tagged JSON.
            if let Ok(json) = serde_json::to_string(&ra.action) {
                yaml.push_str(&format!("    params: {}\n", json));
            }
        }
        yaml
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::CanonicalAction;

    fn make_actions(n: u32) -> Vec<ReproAction> {
        (0..n)
            .map(|i| ReproAction {
                index: i,
                action: CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Tab),
                },
            })
            .collect()
    }

    #[test]
    fn test_minimize_already_minimal() {
        // Test function: only [action_2, action_5] reproduces
        let test_fn = |actions: &[ReproAction]| {
            let has_2 = actions.iter().any(|a| a.index == 2);
            let has_5 = actions.iter().any(|a| a.index == 5);
            ReproResult {
                actions_executed: actions.len() as u32,
                reproduced: has_2 && has_5,
                failure_type: if has_2 && has_5 {
                    Some("crash".into())
                } else {
                    None
                },
            }
        };

        let minimizer = ReproMinimizer::new(test_fn);
        let actions = make_actions(10);
        let minimal = minimizer.minimize(&actions);

        assert_eq!(minimal.len(), 2);
        assert!(minimal.iter().any(|a| a.index == 2));
        assert!(minimal.iter().any(|a| a.index == 5));
    }

    #[test]
    fn test_minimize_no_reproduction() {
        let test_fn = |_actions: &[ReproAction]| ReproResult {
            actions_executed: 0,
            reproduced: false,
            failure_type: None,
        };

        let minimizer = ReproMinimizer::new(test_fn);
        let actions = make_actions(5);
        let minimal = minimizer.minimize(&actions);

        // Should return original if can't reproduce
        assert_eq!(minimal.len(), 5);
    }

    #[test]
    fn test_minimize_single_action() {
        let test_fn = |actions: &[ReproAction]| {
            let has_3 = actions.iter().any(|a| a.index == 3);
            ReproResult {
                actions_executed: actions.len() as u32,
                reproduced: has_3,
                failure_type: if has_3 { Some("fail".into()) } else { None },
            }
        };

        let minimizer = ReproMinimizer::new(test_fn);
        let actions = make_actions(10);
        let minimal = minimizer.minimize(&actions);

        assert_eq!(minimal.len(), 1);
        assert_eq!(minimal[0].index, 3);
    }

    #[test]
    fn test_minimize_empty() {
        let test_fn = |_actions: &[ReproAction]| ReproResult {
            actions_executed: 0,
            reproduced: true,
            failure_type: None,
        };

        let minimizer = ReproMinimizer::new(test_fn);
        let minimal = minimizer.minimize(&[]);

        assert!(minimal.is_empty());
    }

    #[test]
    fn test_to_yaml() {
        let test_fn = |_actions: &[ReproAction]| ReproResult {
            actions_executed: 0,
            reproduced: true,
            failure_type: None,
        };

        let minimizer = ReproMinimizer::new(test_fn);
        let actions = vec![
            ReproAction {
                index: 0,
                action: CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Tab),
                },
            },
            ReproAction {
                index: 5,
                action: CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Enter),
                },
            },
        ];

        let yaml = minimizer.to_yaml(&actions);
        assert!(yaml.contains("schema: tui-lab/repro/v1"));
        assert!(yaml.contains("actions: 2"));
        // Names come from the canonical action; the replayable payload is
        // embedded as tagged JSON (Wave-2 item 10).
        assert!(yaml.contains("action: key"));
        assert!(yaml.contains(r#""kind":"key""#));
        assert!(yaml.contains("index: 5"));
    }
}
