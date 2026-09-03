//! Coverage-guided exploration (re-review item 29).
//!
//! Random and semantic exploration both count "novel states", but the old
//! novelty signal was a single dimension — have I seen this state identity
//! before. A screen that changes one character is novel; a screen that
//! reveals a whole new focus edge, control, coverage target, or terminal
//! mode is not distinguished from a repeat. This module makes novelty
//! MULTI-DIMENSIONAL and evidence-carrying:
//!
//! * **semantic state** — the layered state identity (unchanged semantics,
//!   now one signal among several);
//! * **focus edge** — a `(from, to, via)` transition the interaction has
//!   never produced;
//! * **control** — a semantic control id never seen in the run;
//! * **coverage target** — a native coverage target the app has not yet
//!   reported (when the app cooperates);
//! * **mode state** — a terminal-mode tri-state change (`mouse_sgr_encoding:
//!   Unknown → Enabled`), derived from the raw protocol timeline.
//!
//! The ledger is the exploration's scoreboard: each dimension reports
//! whether the last action moved it, and the report carries per-dimension
//! counts so a caller sees WHAT was explored, not just how many steps ran.

use crate::exploration::state_graph::StateIdentity;
use std::collections::BTreeSet;

/// One novelty dimension's contribution from the last step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NoveltySignal {
    pub dimension: &'static str,
    /// Whether this step moved the dimension (a new key appeared).
    pub novel: bool,
    /// The key that was new, when novel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Per-step novelty outcome across all dimensions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StepNovelty {
    pub signals: Vec<NoveltySignal>,
    /// True when at least one dimension moved.
    pub any_novel: bool,
}

impl StepNovelty {
    #[allow(dead_code)]
    fn none() -> Self {
        StepNovelty {
            signals: Vec::new(),
            any_novel: false,
        }
    }
}

/// The run-wide novelty scoreboard (item 29). Feed `note` after every
/// action with the fused identity, the focus graph, the semantic screen,
/// the run's native coverage targets, and the folded mode states.
#[derive(Debug, Default)]
pub struct NoveltyLedger {
    state_keys: BTreeSet<String>,
    focus_edges: BTreeSet<String>,
    controls: BTreeSet<String>,
    coverage_targets: BTreeSet<String>,
    mode_states: BTreeSet<String>,
}

impl NoveltyLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one post-action observation and return what it made novel.
    /// Every argument is optional so partial evidence still participates —
    /// a non-cooperative app just contributes fewer dimensions.
    pub fn note(
        &mut self,
        identity: Option<&StateIdentity>,
        focus_edges: &[(String, String, String)],
        controls: &[String],
        coverage_targets: &[String],
        mode_states: &[(String, crate::protocol::KnownModeState)],
    ) -> StepNovelty {
        let mut signals = Vec::new();

        if let Some(id) = identity {
            let key = id.id().as_str().to_string();
            let novel = self.state_keys.insert(key.clone());
            signals.push(NoveltySignal {
                dimension: "semantic_state",
                novel,
                key: novel.then_some(key),
            });
        }

        for (from, to, via) in focus_edges {
            let key = format!("{via}:{from}->{to}");
            if self.focus_edges.insert(key.clone()) {
                signals.push(NoveltySignal {
                    dimension: "focus_edge",
                    novel: true,
                    key: Some(key),
                });
            }
        }

        for c in controls {
            if self.controls.insert(c.clone()) {
                signals.push(NoveltySignal {
                    dimension: "control",
                    novel: true,
                    key: Some(c.clone()),
                });
            }
        }

        for t in coverage_targets {
            if self.coverage_targets.insert(t.clone()) {
                signals.push(NoveltySignal {
                    dimension: "coverage_target",
                    novel: true,
                    key: Some(t.clone()),
                });
            }
        }

        for (mode, state) in mode_states {
            // Unknown participates: an observed move out of Unknown IS new
            // knowledge (item 20's honesty rule applied to novelty — "we
            // learned the mode state" is progress even when it stays off).
            let key = format!("{}={:?}", mode, state);
            if self.mode_states.insert(key.clone()) {
                signals.push(NoveltySignal {
                    dimension: "mode_state",
                    novel: true,
                    key: Some(key),
                });
            }
        }

        let any_novel = signals.iter().any(|s| s.novel);
        StepNovelty { signals, any_novel }
    }

    /// Total distinct keys per dimension — the exploration's coverage
    /// scoreboard.
    pub fn summary(&self) -> NoveltySummary {
        NoveltySummary {
            semantic_states: self.state_keys.len(),
            focus_edges: self.focus_edges.len(),
            controls: self.controls.len(),
            coverage_targets: self.coverage_targets.len(),
            mode_states: self.mode_states.len(),
        }
    }
}

/// Run-wide per-dimension totals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NoveltySummary {
    pub semantic_states: usize,
    pub focus_edges: usize,
    pub controls: usize,
    pub coverage_targets: usize,
    pub mode_states: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(tag: &str) -> StateIdentity {
        StateIdentity::from_parts(tag)
    }

    #[test]
    fn dimensions_move_independently() {
        let mut ledger = NoveltyLedger::new();

        // Step 1: new state, new control, new mode fact.
        let n1 = ledger.note(
            Some(&identity("s1")),
            &[],
            &["button:save".to_string()],
            &[],
            &[(
                "mouse_sgr_encoding".to_string(),
                crate::protocol::KnownModeState::Enabled,
            )],
        );
        assert!(n1.any_novel);
        assert_eq!(n1.signals.len(), 3);

        // Step 2: SAME state but a new focus edge — still novel.
        let n2 = ledger.note(
            Some(&identity("s1")),
            &[("a".into(), "b".into(), "tab".into())],
            &["button:save".to_string()],
            &[],
            &[(
                "mouse_sgr_encoding".to_string(),
                crate::protocol::KnownModeState::Enabled,
            )],
        );
        assert!(
            n2.any_novel,
            "a new focus edge is novelty even in a seen state"
        );
        let state_signal = n2
            .signals
            .iter()
            .find(|s| s.dimension == "semantic_state")
            .unwrap();
        assert!(!state_signal.novel);
        assert_eq!(
            n2.signals
                .iter()
                .filter(|s| s.dimension == "focus_edge")
                .count(),
            1
        );

        // Step 3: nothing new — no novelty.
        let n3 = ledger.note(
            Some(&identity("s1")),
            &[("a".into(), "b".into(), "tab".into())],
            &["button:save".to_string()],
            &[],
            &[(
                "mouse_sgr_encoding".to_string(),
                crate::protocol::KnownModeState::Enabled,
            )],
        );
        assert!(!n3.any_novel);
    }

    /// Item 20's honesty rule carried into novelty: moving a mode OUT of
    /// Unknown is new knowledge even when the mode stays disabled.
    #[test]
    fn mode_unknown_to_disabled_is_novel() {
        let mut ledger = NoveltyLedger::new();
        let n1 = ledger.note(
            None,
            &[],
            &[],
            &[],
            &[(
                "alt_screen".to_string(),
                crate::protocol::KnownModeState::Unknown,
            )],
        );
        assert!(n1.any_novel);
        let n2 = ledger.note(
            None,
            &[],
            &[],
            &[],
            &[(
                "alt_screen".to_string(),
                crate::protocol::KnownModeState::Disabled,
            )],
        );
        assert!(
            n2.signals
                .iter()
                .any(|s| s.dimension == "mode_state" && s.novel),
            "Unknown→Disabled is a learned fact"
        );
    }

    #[test]
    fn summary_counts_distinct_keys() {
        let mut ledger = NoveltyLedger::new();
        let _ = ledger.note(
            Some(&identity("a")),
            &[("x".into(), "y".into(), "tab".into())],
            &["c1".to_string(), "c2".to_string()],
            &["widget:save".to_string()],
            &[],
        );
        let _ = ledger.note(
            Some(&identity("b")),
            &[("x".into(), "y".into(), "tab".into())],
            &["c2".to_string()],
            &["widget:save".to_string()],
            &[],
        );
        let s = ledger.summary();
        assert_eq!(s.semantic_states, 2);
        assert_eq!(s.focus_edges, 1, "duplicate edge is not re-counted");
        assert_eq!(s.controls, 2);
        assert_eq!(s.coverage_targets, 1);
    }
}
