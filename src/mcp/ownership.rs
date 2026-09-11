//! Server-scoped ownership and plan registries (god-object round 2, G5).
//!
//! Two `Arc<Mutex<HashMap>>` fields move OUT of the `TuiLabServer`
//! struct into dedicated store types. The server keeps the same Arc
//! handles (clone-shared with handler child modules that cannot capture
//! `&self` across an actor `.await`), so no lock discipline changes:
//! still short locks, never held across an `.await` (invariant 2).
//!
//! - [`SessionOwnership`]: which run each live session belongs to
//!   (review P0.2). Run provenance is a server concern, not a session
//!   concern — a session is only usable while it belongs to the CURRENT
//!   run.
//! - [`IntentPlanStore`]: previewed execution plans by plan_id (finding
//!   3D). `plan` stores; [`IntentPlanStore::consume`] revalidates +
//!   removes — a plan executes at most once, so a replayed plan_id
//!   cannot re-fire a destructive payload. The at-most-once rule is
//!   STORE policy now, not handler discipline.

/// One previewed intent plan: the control identity it resolved against,
/// kept between `tui_intent plan` and `tui_intent execute`. Risk is NOT
/// stored here — the executor re-fences against the plan's LIVE risk at
/// execute time, never a stale snapshot.
#[derive(Clone)]
pub(crate) struct IntentPlanTicket {
    pub(crate) control_id: String,
    /// Beta-audit P0-8: the FULL plan binding. A plan_id authorizes the
    /// plan the caller previewed — the session, verb, risk class, and the
    /// step shape are all checked at execute time, so a caller cannot
    /// preview verb A and submit the plan_id for verb B (the old ticket
    /// bound only the control identity).
    pub(crate) session_id: String,
    pub(crate) verb: String,
    pub(crate) risk: String,
    /// Hash of the plan's serialized steps (the exact execution shape).
    pub(crate) steps_hash: u64,
    /// Bounded store: creation order, oldest evicted first.
    pub(crate) created: std::time::Instant,
}

/// Beta-audit P0-8: a previewed plan expires. The stale_plan /
/// expired-plan error semantics always claimed plans had a bounded
/// lifetime; now one is enforced (generous — plans are previewed and
/// executed back-to-back, not archived).
pub(crate) const INTENT_PLAN_TTL: std::time::Duration = std::time::Duration::from_secs(300);

impl IntentPlanTicket {
    /// Whether this ticket has expired (age beyond the TTL).
    pub(crate) fn expired(&self) -> bool {
        self.created.elapsed() > INTENT_PLAN_TTL
    }

    /// The plan fingerprint this ticket authorizes: control + verb + risk
    /// + step shape. Execute-time resolution must produce the same
    ///   fingerprint or the ticket refuses.
    pub(crate) fn fingerprint(
        &self,
        control_id: &str,
        verb: &str,
        risk: &str,
        steps_hash: u64,
    ) -> bool {
        self.control_id == control_id
            && self.verb == verb
            && self.risk == risk
            && self.steps_hash == steps_hash
    }
}

/// Hash a plan's step list into the ticket's step fingerprint (P0-8).
/// `DefaultHasher` is stable within one process lifetime, which is the
/// ticket's whole life (plans are server-side state and never persisted).
pub(crate) fn plan_steps_hash(steps_json: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    steps_json.to_string().hash(&mut h);
    h.finish()
}

/// Bounded intent-plan store (finding 3D): at most this many previewed
/// plans are held; the oldest is evicted first.
pub(crate) const INTENT_PLAN_CAP: usize = 64;

/// Which run each live session belongs to.
#[derive(Default)]
pub(crate) struct SessionOwnership {
    map: std::collections::HashMap<String, String>,
}

impl SessionOwnership {
    pub(crate) fn new() -> Self {
        SessionOwnership::default()
    }

    /// Bind a session to a run (launch/attach time).
    pub(crate) fn bind(&mut self, session: &str, run: &str) {
        self.map.insert(session.to_string(), run.to_string());
    }

    /// The owning run of a session, if bound.
    pub(crate) fn owner_of(&self, session: &str) -> Option<String> {
        self.map.get(session).cloned()
    }

    /// Whether the session's owner is exactly `run`.
    pub(crate) fn owned_by(&self, session: &str, run: &str) -> bool {
        self.map.get(session).map(|o| o.as_str()) == Some(run)
    }

    /// Sessions owned by `run` (live sessions carrying that binding).
    pub(crate) fn sessions_of(&self, live: &[String], run: &str) -> Vec<String> {
        live.iter()
            .filter(|sid| self.owned_by(sid, run))
            .cloned()
            .collect()
    }

    /// Sessions NOT owned by `run`. `None` owners count as foreign:
    /// an unbound session must be re-launched (or re-attached) under a
    /// run before driving it.
    pub(crate) fn foreign_sessions(&self, live: &[String], run: &str) -> Vec<String> {
        live.iter()
            .filter(|sid| !self.owned_by(sid, run))
            .cloned()
            .collect()
    }

    /// Drop a session's binding (stop/detach time).
    pub(crate) fn unbind(&mut self, session: &str) {
        self.map.remove(session);
    }
}

/// The bounded previewed-intent-plan store.
#[derive(Default)]
pub(crate) struct IntentPlanStore {
    map: std::collections::HashMap<String, IntentPlanTicket>,
}

impl IntentPlanStore {
    pub(crate) fn new() -> Self {
        IntentPlanStore::default()
    }

    /// Store a previewed plan. Bounded: at [`INTENT_PLAN_CAP`] the
    /// oldest ticket is evicted first (declared eviction, same policy
    /// shape as every other bounded store here).
    pub(crate) fn store(&mut self, plan_id: String, ticket: IntentPlanTicket) {
        if self.map.len() >= INTENT_PLAN_CAP {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, t)| t.created)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(plan_id, ticket);
    }

    /// Consume a plan by id: remove-and-return. A plan executes at most
    /// once — a replayed plan_id finds the store empty. Beta-audit P0-8:
    /// an expired ticket is dropped on read (the caller sees
    /// `unknown_plan`, the honest answer for a preview that outlived its
    /// window).
    pub(crate) fn consume(&mut self, plan_id: &str) -> Option<IntentPlanTicket> {
        match self.map.remove(plan_id) {
            Some(t) if !t.expired() => Some(t),
            Some(_) => None,
            None => None,
        }
    }

    /// How many plans are currently held (test introspection).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_tracks_bind_query_unbind() {
        let mut own = SessionOwnership::new();
        own.bind("s1", "run-a");
        own.bind("s2", "run-b");
        assert_eq!(own.owner_of("s1").as_deref(), Some("run-a"));
        assert!(own.owned_by("s1", "run-a"));
        assert!(!own.owned_by("s1", "run-b"));
        let live = vec!["s1".to_string(), "s2".to_string(), "s3".to_string()];
        assert_eq!(own.sessions_of(&live, "run-a"), vec!["s1".to_string()]);
        // Unbound sessions count as foreign (never silently adopted).
        assert_eq!(
            own.foreign_sessions(&live, "run-a"),
            vec!["s2".to_string(), "s3".to_string()]
        );
        own.unbind("s1");
        assert_eq!(own.owner_of("s1"), None);
    }

    #[test]
    fn plan_store_evicts_oldest_at_cap() {
        let mut store = IntentPlanStore::new();
        for i in 0..INTENT_PLAN_CAP {
            store.store(
                format!("p{i}"),
                IntentPlanTicket {
                    control_id: "c".into(),
                    session_id: "s".into(),
                    verb: "activate".into(),
                    risk: "mutating".into(),
                    steps_hash: 0,
                    created: std::time::Instant::now(),
                },
            );
        }
        assert_eq!(store.len(), INTENT_PLAN_CAP);
        // One more evicts the oldest (p0), never overflows.
        store.store(
            "late".to_string(),
            IntentPlanTicket {
                control_id: "c".into(),
                session_id: "s".into(),
                verb: "activate".into(),
                risk: "mutating".into(),
                steps_hash: 0,
                created: std::time::Instant::now(),
            },
        );
        assert_eq!(store.len(), INTENT_PLAN_CAP);
        assert!(store.consume("p0").is_none(), "oldest was evicted");
        assert!(store.consume("late").is_some());
    }

    #[test]
    fn consume_is_at_most_once() {
        let mut store = IntentPlanStore::new();
        store.store(
            "p".to_string(),
            IntentPlanTicket {
                control_id: "c".into(),
                session_id: "s".into(),
                verb: "activate".into(),
                risk: "mutating".into(),
                steps_hash: 0,
                created: std::time::Instant::now(),
            },
        );
        let first = store.consume("p");
        assert!(first.is_some());
        assert!(store.consume("p").is_none(), "replay finds the store empty");
    }
}
