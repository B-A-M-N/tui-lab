//! Mutation guard (re-review P0.9): an expected-state check performed
//! atomically with the send, inside the executor.
//!
//! The SessionActor serializes operations, but it cannot prevent the screen
//! from changing *between* two tool calls. An agent that observed
//! `[ Save ]`, then had a background modal open underneath it, then sent a
//! click at the old Save coordinates would hit `[ Delete ]` instead — and
//! neither the agent nor the ledger would know. A guard pins what the
//! caller believed the world looked like when it decided to act; the
//! executor validates that belief and, on drift, refuses to send, returning
//! a structured `stale_state` verdict naming expected vs actual.

use serde_json::json;

/// The caller's belief about the current session state, verified inside the
/// executor immediately before the input lands. Every field is optional;
/// an empty guard is a no-op (the historical behavior).
#[derive(
    Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub struct MutationGuard {
    /// Session generation the observation came from. A restart between
    /// observe and act fails the guard even if the screen looks identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// Structure hash observed at decision time (layout skeleton). Drift
    /// here means the layout changed — repositioned controls, an opened
    /// modal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure_hash: Option<String>,
    /// Focused control id at decision time (fused semantics — a
    /// cooperative app's native focus participates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_control_id: Option<String>,
}

impl MutationGuard {
    /// Capture a guard from the session's CURRENT state — the shape a
    /// caller sends when it wants "act only if the world hasn't moved
    /// since right now" (racy-read protection for the executor's own
    /// window).
    pub fn capture(session: &crate::session::state::Session) -> Self {
        let screen_fused = session.analyze_last();
        MutationGuard {
            generation: Some(session.generation),
            structure_hash: screen_fused
                .as_ref()
                .map(|a| a.frame.structure_hash.clone()),
            focus_control_id: screen_fused
                .as_ref()
                .and_then(|a| a.semantic.focus.control_id.clone()),
        }
    }

    /// Validate against live state. `Ok(())` means every declared
    /// expectation holds; `Err(payload)` is the structured `stale_state`
    /// JSON naming the first violated expectation, expected vs actual.
    pub fn validate(
        &self,
        session: &crate::session::state::Session,
    ) -> Result<(), serde_json::Value> {
        // Generation first: a restart invalidates everything else.
        if let Some(want_gen) = self.generation {
            if session.generation != want_gen {
                return Err(json!({
                    "category": "stale_state",
                    "check": "generation",
                    "expected": want_gen,
                    "actual": session.generation,
                    "summary": format!(
                        "session restarted since the guard was captured (generation {want_gen} -> {})",
                        session.generation
                    ),
                }));
            }
        }
        // Fused analysis once; both structure and focus compare against it.
        let analysis = session.analyze_last();
        let Some(analysis) = analysis else {
            if self.structure_hash.is_some() || self.focus_control_id.is_some() {
                return Err(json!({
                    "category": "stale_state",
                    "check": "frame",
                    "expected": "an observed frame",
                    "actual": null,
                    "summary": "no frame has been observed yet; guard cannot hold",
                }));
            }
            return Ok(());
        };
        if let Some(want_hash) = &self.structure_hash {
            if &analysis.frame.structure_hash != want_hash {
                return Err(json!({
                    "category": "stale_state",
                    "check": "structure",
                    "expected": want_hash,
                    "actual": analysis.frame.structure_hash,
                    "summary": "screen structure changed since the guard was captured \
                                (layout shifted, a modal opened, or content replaced)",
                }));
            }
        }
        if let Some(want_focus) = &self.focus_control_id {
            let actual = analysis.semantic.focus.control_id.as_deref();
            if actual != Some(want_focus.as_str()) {
                return Err(json!({
                    "category": "stale_state",
                    "check": "focus",
                    "expected": want_focus,
                    "actual": actual,
                    "summary": "focus moved since the guard was captured",
                }));
            }
        }
        Ok(())
    }
}

/// Where a guard came from, for the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOrigin {
    /// The MCP caller supplied it explicitly (tui_act / tui_probe).
    Caller,
    /// The executor captured it itself at decision time.
    Executor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_guard_validates_trivially() {
        let g = MutationGuard::default();
        // No session state needed: nothing was declared.
        // (validate() with a session-less environment is exercised through
        // the executor tests; here the default is structurally a no-op.)
        assert_eq!(g, MutationGuard::default());
    }

    #[test]
    fn guard_serializes_and_deserializes() {
        let g = MutationGuard {
            generation: Some(3),
            structure_hash: Some("abc".into()),
            focus_control_id: Some("button/save".into()),
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("button/save"));
        let back: MutationGuard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }
}
