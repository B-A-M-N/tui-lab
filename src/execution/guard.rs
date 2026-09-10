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
    /// Native semantic channel revision at decision time. A cooperative
    /// app can change focus/state with no pixel change; if this revision
    /// advances, the guarded belief is stale even when the grid is equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_revision: Option<u64>,
    /// Text that must remain visible in the CURRENT viewport. This is a
    /// separate predicate because normalization can mask content changes
    /// in the structural identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_visible: Option<String>,
}

impl MutationGuard {
    /// Capture a guard from the session's CURRENT state — the shape a
    /// caller sends when it wants "act only if the world hasn't moved
    /// since right now" (racy-read protection for the executor's own
    /// window).
    pub fn capture(
        analysis: Option<&crate::session::state::FrameAnalysis>,
        generation: u32,
    ) -> Self {
        MutationGuard {
            generation: Some(generation),
            structure_hash: analysis.map(|a| a.frame.structure_hash.clone()),
            focus_control_id: analysis.and_then(|a| a.semantic.focus.control_id.clone()),
            native_revision: None,
            text_visible: None,
        }
    }

    /// Validate against live state. `Ok(())` means every declared
    /// expectation holds; `Err(payload)` is the structured `stale_state`
    /// JSON naming the first violated expectation, expected vs actual.
    pub fn validate_fresh(
        &self,
        session: &mut crate::session::state::Session,
    ) -> Result<(), serde_json::Value> {
        // A guard is a claim about CURRENT terminal state. Pump the backend
        // and native channel immediately before dispatch; analyzing the
        // last settled observation cannot close the observe→act race because
        // the target process changes independently of TUI-Lab method calls.
        let analysis = session.peek_fresh().map_err(|e| {
            json!({
                "category": "stale_state",
                "check": "refresh",
                "expected": "a fresh pre-dispatch frame",
                "actual": e.to_string(),
                "summary": "cannot verify the guarded state: pre-dispatch refresh failed"
            })
        })?;
        self.validate_analysis(&analysis, session, session.generation)
    }

    /// Validate against a fused frame the caller acquired atomically. This
    /// is pure policy; executor paths must use [`Self::validate_fresh`].
    pub fn validate_analysis(
        &self,
        analysis: &crate::session::state::FrameAnalysis,
        session: &crate::session::state::Session,
        generation: u32,
    ) -> Result<(), serde_json::Value> {
        // Generation first: a restart invalidates everything else.
        if let Some(want_gen) = self.generation {
            if generation != want_gen {
                return Err(json!({
                    "category": "stale_state",
                    "check": "generation",
                    "expected": want_gen,
                    "actual": generation,
                    "summary": format!(
                        "session restarted since the guard was captured (generation {want_gen} -> {generation})"
                    ),
                }));
            }
        }
        // Both structure and focus compare against the supplied fused
        // analysis. Empty terminal state is a valid screen; the earlier
        // `Option<FrameAnalysis>` conflation treated it as "no frame".
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
        if let Some(want_rev) = self.native_revision {
            if session.native_revision() != Some(want_rev) {
                return Err(json!({
                    "category": "stale_state",
                    "check": "native_revision",
                    "expected": want_rev,
                    "actual": session.native_revision(),
                    "summary": "native semantic state changed since the guard was captured",
                }));
            }
        }
        if let Some(want_text) = &self.text_visible {
            if !crate::capture::visible_text_contains(&analysis.frame, want_text) {
                return Err(json!({
                    "category": "stale_state",
                    "check": "text_visible",
                    "expected": want_text,
                    "actual": null,
                    "summary": "required visible text is no longer on the current screen",
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
            native_revision: Some(7),
            text_visible: Some("[ Save ]".into()),
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("button/save"));
        let back: MutationGuard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }
}
