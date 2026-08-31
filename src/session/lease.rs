//! Human control lease (Wave G item 76): a human can take exclusive manual
//! control of a running session for a bounded time.
//!
//! While a valid lease is held, every machine-driving tool (`tui_act`,
//! `tui_explore`, `tui_audit`, scenario replay, `tui_record` lifecycle)
//! refuses with `control_leased` — observation (`tui_observe`,
//! `tui_session status`) stays allowed, because watching does not fight the
//! human for input. A lease is a coordination contract between agents and
//! people, not a security primitive: it expires on its own (TTL), and the
//! holder can release early.

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One lease grant on a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlLease {
    /// Who holds it (free-form label from the request — a person's name,
    /// "terminal attached via tmux", etc.).
    pub holder: String,
    /// Unix-millis instant the lease was taken.
    pub taken_at_ms: u64,
    /// Time-to-live in milliseconds from `taken_at_ms`.
    pub ttl_ms: u64,
}

impl ControlLease {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Milliseconds remaining on the lease (0 when expired).
    pub fn remaining_ms(&self) -> u64 {
        let elapsed = Self::now_ms().saturating_sub(self.taken_at_ms);
        self.ttl_ms.saturating_sub(elapsed)
    }

    /// True while the lease is still valid.
    pub fn active(&self) -> bool {
        self.remaining_ms() > 0
    }
}

/// The lease state of one session: `Some(lease)` while held.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeaseState {
    lease: Option<ControlLease>,
}

impl LeaseState {
    /// Take the lease. A fresh grant always wins over an expired one;
    /// a *live* lease must be released (or expired) before a new holder
    /// takes it — silently stealing a human's session defeats the point.
    pub fn acquire(&mut self, holder: &str, ttl_ms: u64) -> Result<ControlLease, ControlLease> {
        if let Some(existing) = &self.lease {
            if existing.active() {
                return Err(existing.clone());
            }
        }
        let lease = ControlLease {
            holder: holder.to_string(),
            taken_at_ms: ControlLease::now_ms(),
            ttl_ms: ttl_ms
                .max(1000)
                .min(Duration::from_secs(3600).as_millis() as u64),
        };
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    /// Release the lease. Returns whether a live lease was actually held.
    pub fn release(&mut self) -> bool {
        match &self.lease {
            Some(l) if l.active() => {
                self.lease = None;
                true
            }
            _ => {
                self.lease = None;
                false
            }
        }
    }

    /// The active lease, if any (expired leases report `None`; the stale
    /// grant is dropped on this read so the state does not accumulate
    /// staleness).
    pub fn active(&mut self) -> Option<ControlLease> {
        let expired = matches!(&self.lease, Some(l) if !l.active());
        if expired {
            self.lease = None;
        }
        self.lease.clone().filter(|l| l.active())
    }

    /// Whether machine driving is currently blocked.
    pub fn blocks_driving(&mut self) -> bool {
        self.active().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_blocks_then_expires() {
        let mut st = LeaseState::default();
        assert!(!st.blocks_driving());
        let l = st.acquire("ana", 5_000).expect("acquire");
        assert_eq!(l.holder, "ana");
        assert!(st.blocks_driving());
        assert!(st.remaining_window() <= 5_000);
        // A second acquire while live is refused, naming the holder.
        let err = st.acquire("bob", 5_000).expect_err("held");
        assert_eq!(err.holder, "ana");
        // Release frees it.
        assert!(st.release());
        assert!(!st.blocks_driving());
        assert!(!st.release(), "second release is a no-op");
    }

    #[test]
    fn expired_lease_can_be_taken_and_reports_none() {
        let mut st = LeaseState {
            lease: Some(ControlLease {
                holder: "ghost".into(),
                taken_at_ms: ControlLease::now_ms() - 60_000,
                ttl_ms: 1_000,
            }),
        };
        assert!(!st.blocks_driving(), "expired lease does not block");
        assert!(st.active().is_none());
        // And a new holder can take it without release.
        st.acquire("new", 5_000).expect("take after expiry");
        assert!(st.blocks_driving());
    }

    #[test]
    fn ttl_is_clamped() {
        let mut st = LeaseState::default();
        let l = st.acquire("x", 0).expect("clamped up to 1s");
        assert!(l.ttl_ms >= 1_000);
        let l2 = st.acquire("x", u64::MAX).expect_err("held");
        let _ = l2;
        st.release();
        let l3 = st.acquire("x", u64::MAX).expect("clamped down to 1h");
        assert!(l3.ttl_ms <= 3_600_000);
    }

    impl LeaseState {
        fn remaining_window(&mut self) -> u64 {
            self.active().map(|l| l.remaining_ms()).unwrap_or(0)
        }
    }
}
