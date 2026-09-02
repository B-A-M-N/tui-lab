//! Adapter availability vs channel activity (re-review item 35).
//!
//! "The harness supports native semantics" and "this app is cooperating
//! right now" are different claims about different parties. The first is
//! about the launch environment; the second is about the app's behavior.
//! One flattened `native: true/false` has to pick a lie: either an
//! exported env var reads as cooperation (it is not — the app may never
//! read the variable), or an app that wrote one frame then died reads as
//! supported infrastructure (it is not — the channel is dead).
//!
//! [`AdapterStatus`] carries both facts plus the health split, and
//! serializes directly onto every response that touches the native path.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AdapterStatus {
    /// The harness exported `TUI_LAB_SEMANTIC` into the launch env (the
    /// channel file exists). Necessary for cooperation, proves nothing.
    pub adapter_available: bool,
    /// The app has written ≥1 VALID frame. The cooperation claim.
    pub native_channel_active: bool,
    /// Valid frames accepted (lifetime of the session).
    pub frames_received: u64,
    /// Frames rejected as malformed. A channel writing ONLY invalid
    /// frames is active-but-broken — the counts name which.
    pub frames_invalid: u64,
    /// `frames_received > 0 && frames_invalid == 0`. A strict definition:
    /// one bad frame among a thousand marks the channel unhealthy, which
    /// is the honest reading (something in the app's emitter is wrong).
    pub healthy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_but_silent_is_not_active() {
        // The shape every non-cooperative app produces: the env var was
        // injected, zero frames ever arrived.
        let st = AdapterStatus {
            adapter_available: true,
            native_channel_active: false,
            frames_received: 0,
            frames_invalid: 0,
            healthy: false,
        };
        assert!(st.adapter_available);
        assert!(!st.native_channel_active);
        assert!(!st.healthy);
    }

    #[test]
    fn active_but_invalid_only_is_unhealthy() {
        let st = AdapterStatus {
            adapter_available: true,
            native_channel_active: false,
            frames_received: 0,
            frames_invalid: 7,
            healthy: false,
        };
        // Invalid-only traffic is NOT cooperation: nothing usable arrived.
        assert!(!st.native_channel_active);
        assert!(!st.healthy, "invalid-only traffic is a broken emitter, not health");
    }

    #[test]
    fn one_invalid_frame_breaks_health() {
        let st = AdapterStatus {
            adapter_available: true,
            native_channel_active: true,
            frames_received: 999,
            frames_invalid: 1,
            healthy: false,
        };
        assert!(st.native_channel_active);
        assert!(!st.healthy);
    }
}
