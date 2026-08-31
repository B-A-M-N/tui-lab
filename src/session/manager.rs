//! **Legacy** synchronous session manager: maps session id -> [`Session`].
//!
//! Do **not** use this for new code. Production uses the actor-backed
//! [`crate::session::SessionPool`] (one OS thread per session with a bounded
//! mailbox), which replaced this global `Mutex`-serializing container —
//! a long audit on session A here blocked every other session, and the
//! blocking `SyncSender` path duplicated the actor's job. This type is kept
//! only because the integration test suite was written against it; new code
//! must route through `SessionPool` (or own a single `Session` directly).
//!
//! Serialized access was justified because Hermes ran with
//! `supports_parallel_tool_calls: false` (spec section 43); the actor model
//! retains that serialization per-session without cross-session blocking.

use std::collections::HashMap;

use crate::session::state::Session;

/// Legacy blocking session registry.
///
/// **Do not use this for new code.** Production routes through
/// [`crate::session::SessionPool`]. This type is kept verbatim (not removed)
/// only because the integration test suite was written against its blocking
/// API; the `#[deprecated]` gate is deliberately omitted so those legacy tests
/// stay warning-free while the module doc states the contract. A future sweep
/// migrates the remaining test call sites to `SessionPool` and deletes this
/// file wholesale.
#[allow(clippy::doc_markdown)]
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    active: Option<String>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
            active: None,
        }
    }

    fn gen_id() -> String {
        // Collision-resistant UUID v4 (spec section 12).
        let id = uuid::Uuid::new_v4();
        format!("sess-{}", id.simple())
    }

    /// Create and start a new session **synchronously** (blocking spawn on
    /// the caller's thread). Returns its id.
    ///
    /// Legacy surface only — `crate::session::SessionPool::start(...).await`
    /// is the production path (launch runs on the actor thread; the async
    /// caller awaits the outcome without blocking a runtime worker).
    // Spec section 13 defines the full 9-arg start surface; refactor to a
    // LaunchSpec-taking variant is tracked with the Wave-3 executor work.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        backend: &str,
        isolation: &str,
    ) -> anyhow::Result<String> {
        let id = Self::gen_id();
        let mut s = Session::new(id.clone(), command.to_string());
        let spec = crate::session::state::LaunchSpec {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(str::to_string),
            env: env.to_vec(),
            cols,
            rows,
            backend: backend.to_string(),
            isolation: isolation.to_string(),
        };
        s.start_with_spec(spec)?;
        self.active = Some(id.clone());
        self.sessions.insert(id.clone(), s);
        Ok(id)
    }

    /// Restart an existing session in place: same id, incremented generation.
    pub fn restart(&mut self, id: &str) -> anyhow::Result<(String, u32)> {
        let sess = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no session '{}'", id))?;
        sess.restart()?;
        Ok((id.to_string(), sess.generation))
    }

    pub fn get(&self, id: &str) -> anyhow::Result<&Session> {
        self.sessions
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no session '{}'", id))
    }

    pub fn get_mut(&mut self, id: &str) -> anyhow::Result<&mut Session> {
        self.sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("no session '{}'", id))
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Resolve which session to operate on: explicit id or the active one.
    pub fn resolve(&self, id: Option<&str>) -> anyhow::Result<&Session> {
        match id {
            Some(i) => self.get(i),
            None => match &self.active {
                Some(a) => self.get(a),
                None => Err(anyhow::anyhow!("no active session")),
            },
        }
    }

    pub fn resolve_mut(&mut self, id: Option<&str>) -> anyhow::Result<&mut Session> {
        match id {
            Some(i) => self.get_mut(i),
            None => match &self.active {
                Some(a) => {
                    let a = a.clone();
                    self.get_mut(&a)
                }
                None => Err(anyhow::anyhow!("no active session")),
            },
        }
    }

    pub fn stop(&mut self, id: &str) -> anyhow::Result<()> {
        if let Some(s) = self.sessions.get_mut(id) {
            s.stop()?;
        }
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        self.sessions.remove(id);
        Ok(())
    }

    pub fn list(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}
