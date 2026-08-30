//! Session manager: maps session id -> [`Session`]. Serialized access because
//! Hermes runs with `supports_parallel_tool_calls: false` (spec section 43).

use std::collections::HashMap;

use crate::session::state::Session;

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

    /// Create and start a new session. Returns its id.
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
