//! Run lifecycle admission (beta-audit P0.1).
//!
//! `tui_run new`/`resume`/`close` REPLACE or RETIRE the live
//! [`crate::run::RunContext`]. Before the coordinator, those swaps were
//! not serialized against ordinary session operations: `new` swapped
//! the context, released the lock, then flushed — leaving a real
//! interval where other calls could observe and use the new run while
//! the transition could still fail; `resume` flushed the live run well
//! before its final swap, so activity landing after the "final" flush
//! vanished with the discarded context.
//!
//! The coordinator is one async admission gate: ordinary operations
//! hold a SHARED lease across their whole admit → execute → commit
//! span; lifecycle transitions take the EXCLUSIVE lease, which blocks
//! new admissions and drains in-flight ones before the swap. A swap
//! can only land where no other operation is inside its critical
//! section — and an operation can only enter its critical section
//! when no transition is in progress.
//!
//! Lock discipline is unchanged and orthogonal: the run `std::Mutex`
//! stays short-lock (never across an await). The lifecycle lease is a
//! `tokio::sync::RwLock` — its guards are held across awaits by
//! design, and its fair write-preferring queue makes the drain
//! deterministic: a waiting writer collects the leases of every
//! operation that arrives after it, so the transition starts only
//! when the LAST in-flight operation releases.
//!
//! This is the same admission model the [`crate::execution::RunTicket`]
//! verifies at commit time: the ticket is the belt (evidence is
//! dropped rather than misattributed if a switch sneaks through
//! anyway); the coordinator is the suspenders (a switch cannot land
//! mid-operation at all). Authorization (`with_sess_authorized`)
//! remains the single authoritative admission boundary — it acquires
//! the shared lease as part of admission.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

/// One admission lease into the current run. Holding it is what keeps
/// a run switch from landing under an in-flight operation.
///
/// The lease carries the lifecycle epoch captured at admission; a
/// commit path can compare it against the live epoch to prove the
/// operation was WHOLLY part of one run era ("no switch crossed me").
/// The ticket does the same against run ids; the epoch is the
/// coordinator-level witness, useful in tests where no run swap (and
/// so no ticket refusal) has happened yet.
#[derive(Clone)]
pub(crate) struct Lease {
    _guard: Arc<OwnedGuard>,
    // Test-only bookkeeping (in-flight counts + drop release); in
    // non-test builds the lease is just the guard + epoch witness, so
    // these handles collapse away.
    #[cfg(test)]
    in_flight: Arc<Mutex<InFlight>>,
    #[cfg(test)]
    label: Arc<Mutex<Option<String>>>,
    // Read in test builds (barrier-witness assertions); the field
    // rides every lease in prod so the shape is identical across cfg.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) captured_epoch: u64,
}

// The two owned guard shapes behind one lease (kept as an enum so a
// lease is one value regardless of kind; drop of the LAST Arc handle
// releases the slot — the guard IS the slot, so its "unused" read is
// its Drop).
#[allow(dead_code)] // Shared's guard is never read, only dropped
enum OwnedGuard {
    Shared(tokio::sync::OwnedRwLockReadGuard<()>),
    Exclusive(tokio::sync::OwnedRwLockWriteGuard<()>),
}

/// Which operations hold shared leases right now — observability for
/// the transition path (a lifecycle handler can name what it drained)
/// and for tests asserting admission counts. `BTreeMap` keeps the
/// listing stable for response payloads.
pub(crate) type InFlight = BTreeMap<String, u64>;

/// The admission gate. Cloned Arc into the server; cheap to share.
#[derive(Clone)]
pub(crate) struct RunLifecycleCoordinator {
    gate: Arc<RwLock<()>>,
    /// Per-label lease counts — test observability for the
    /// deterministic drain assertions (who is admitted when).
    #[cfg_attr(not(test), allow(dead_code))]
    in_flight: Arc<Mutex<InFlight>>,
    /// Bumped when an exclusive section BEGINS (not when it ends —
    /// the era changes at the start of the transition, which is when
    /// the run context is conceptually replaced).
    epoch: Arc<AtomicU64>,
}

impl Default for RunLifecycleCoordinator {
    fn default() -> Self {
        RunLifecycleCoordinator {
            gate: Arc::new(RwLock::new(())),
            in_flight: Arc::new(Mutex::new(BTreeMap::new())),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl RunLifecycleCoordinator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The current lifecycle epoch. Test witness for "wholly in A or
    /// wholly in B" assertions; also stamped into lifecycle
    /// responses so an agent can correlate a transition with the
    /// evidence era it produced.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Shared lease for one ordinary operation (a driving call, an
    /// observe, a session start...). Awaits any in-flight transition.
    /// `op` labels the holder in the in-flight map.
    pub(crate) async fn shared(&self, op: &str) -> Lease {
        // Capture the epoch BEFORE the gate so a transition that bumps
        // it after this read but before the read-guard lands is VISIBLE
        // to the commit comparison (conservative: prefer reporting a
        // crossed switch over missing one). After the read guard is
        // held, no transition can be in progress and the epoch is
        // stable for the lease's lifetime — re-read under the guard
        // for the authoritative value.
        let _pre = self.epoch.load(Ordering::SeqCst);
        let guard = self.gate.clone().read_owned().await;
        let captured = self.epoch.load(Ordering::SeqCst);
        let lease = Lease {
            _guard: Arc::new(OwnedGuard::Shared(guard)),
            #[cfg(test)]
            in_flight: self.in_flight.clone(),
            #[cfg(test)]
            label: Arc::new(Mutex::new(None)),
            captured_epoch: captured,
        };
        lease.enter(op);
        lease
    }

    /// Exclusive lease for a lifecycle transition (`new`/`resume`/
    /// `close`). Blocks new admissions, then drains: the acquire
    /// returns only when every in-flight shared lease has been
    /// released. The epoch bumps BEFORE the caller starts mutating
    /// state, so any operation comparing its captured epoch sees the
    /// transition even before the run id changes.
    pub(crate) async fn exclusive(&self, op: &str) -> Lease {
        let guard = self.gate.clone().write_owned().await;
        let lease = Lease {
            _guard: Arc::new(OwnedGuard::Exclusive(guard)),
            #[cfg(test)]
            in_flight: self.in_flight.clone(),
            #[cfg(test)]
            label: Arc::new(Mutex::new(None)),
            captured_epoch: self.epoch.load(Ordering::SeqCst),
        };
        self.epoch.fetch_add(1, Ordering::SeqCst);
        lease.enter(op);
        lease
    }

    /// What is in flight right now (per operation label). Under the
    /// exclusive lease a lifecycle handler can report exactly what it
    /// drained; steady state it is observability only. Test-only for
    /// now: the close-path response reports drained sessions through
    /// its own summary instead.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> InFlight {
        self.in_flight
            .lock()
            .expect("coordinator in_flight lock")
            .clone()
    }

    /// A test-visibility probe: can an exclusive transition start
    /// right now? True when no shared lease is held. Used by the
    /// deterministic barrier tests to prove admission actually
    /// blocked, without polling run ids.
    #[cfg(test)]
    pub(crate) fn admits_exclusive_now(&self) -> bool {
        // try_write succeeds only when no read guard is held (tokio's
        // RwLock is write-preferring but try_write still reflects
        // current readers).
        match self.gate.clone().try_write_owned() {
            Ok(g) => {
                drop(g);
                true
            }
            Err(_) => false,
        }
    }
}

impl Lease {
    /// Record this lease under `op` in the in-flight map; drop
    /// releases the slot. Split from the constructor so the lease
    /// owns its own bookkeeping. Test-only: the map exists for
    /// deterministic drain assertions; production responses report
    /// outcomes, not lease counts.
    #[cfg(test)]
    fn enter(&self, op: &str) {
        *self
            .in_flight
            .lock()
            .expect("coordinator in_flight lock")
            .entry(op.to_string())
            .or_insert(0) += 1;
        *self.label.lock().expect("lease label lock") = Some(op.to_string());
    }

    #[cfg(not(test))]
    fn enter(&self, _op: &str) {}
}

impl Drop for Lease {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(label) = self.label.lock().expect("lease label lock").take() {
            let mut map = self.in_flight.lock().expect("coordinator in_flight lock");
            if let Some(c) = map.get_mut(&label) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    map.remove(&label);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_leases_block_exclusive_until_released() {
        let c = RunLifecycleCoordinator::new();
        let l1 = c.shared("op-a").await;
        let l2 = c.shared("op-a").await;
        assert!(!c.admits_exclusive_now());
        drop(l1);
        drop(l2);
        assert!(c.admits_exclusive_now());
    }

    #[tokio::test]
    async fn exclusive_blocks_new_shared() {
        let c = RunLifecycleCoordinator::new();
        let x = c.exclusive("run-new").await;
        assert!(!c.admits_exclusive_now());
        drop(x);
        assert!(c.admits_exclusive_now());
    }

    #[tokio::test]
    async fn epoch_bumps_per_transition() {
        let c = RunLifecycleCoordinator::new();
        let l = c.shared("op").await;
        assert_eq!(l.captured_epoch, 0);
        drop(l);
        let _x = c.exclusive("run-new").await;
        assert_eq!(c.epoch(), 1);
    }

    #[tokio::test]
    async fn in_flight_counts_track_leases() {
        let c = RunLifecycleCoordinator::new();
        {
            let _l = c.shared("drive").await;
            assert_eq!(c.in_flight().get("drive"), Some(&1));
        }
        assert_eq!(c.in_flight().get("drive"), None);
    }
}
