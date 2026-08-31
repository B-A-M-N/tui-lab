//! Session actors (Wave G item 73): one thread-backed actor per session,
//! replacing the single global blocking `Mutex<SessionManager>`.
//!
//! The old shape held one std Mutex around EVERY session for the whole
//! duration of a handler — a long audit on session A blocked a `tui_observe`
//! on session B, and the run/close path had to dance around guard
//! re-entrancy (the "close-path deadlock" fix). Hermes runs without
//! parallel tool calls today, but the serialization was at the wrong
//! granularity: work on one session has no reason to exclude work on
//! another.
//!
//! The actor model:
//!
//! - each session is owned by a dedicated OS thread with a bounded mailbox;
//! - callers send a closure `FnOnce(&mut Session) -> R + Send` and await a
//!   oneshot reply. The closure runs to completion on the actor thread, so
//!   session I/O (PTY waits, settles) never blocks the async runtime or any
//!   other session;
//! - the directory (id → mailbox) is a short-lock `RwLock`; the active-id
//!   pointer is a tiny Mutex. Neither is ever held across an await;
//! - the run-context lock is NEVER held across an actor call (handlers
//!   record into the run after the reply arrives) — the rule the old
//!   close-path bug proved necessary.
//!
//! Long multi-step drivers (audits, exploration) submit ONE closure that
//! runs the whole loop inside the actor: the session is legitimately busy
//! for the duration, and every other session stays live.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, RwLock};

use crate::session::state::{LaunchSpec, Session};

/// The message an actor understands: run this closure against the session,
/// reply with its result.
type Job = Box<dyn FnOnce(&mut Session) + Send + 'static>;

enum Mail {
    /// Execute a job; the closure signals the caller's oneshot itself.
    Job(Job),
    /// Stop the actor thread after draining (session stop dropped the last
    /// handle, or an explicit shutdown).
    Shutdown,
}

/// One session's actor: a cloneable sender plus a join handle.
pub struct SessionActor {
    id: String,
    tx: mpsc::SyncSender<Mail>,
    /// Reader thread join handle, taken by `shutdown`.
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Clone for SessionActor {
    fn clone(&self) -> Self {
        SessionActor {
            id: self.id.clone(),
            tx: self.tx.clone(),
            join: Mutex::new(None),
        }
    }
}

impl SessionActor {
    /// Spawn an actor thread owning `session`. Returns the handle.
    pub fn spawn(session: Session) -> SessionActor {
        let id = session.id.clone();
        let (tx, rx) = mpsc::sync_channel::<Mail>(64);
        let join = std::thread::Builder::new()
            .name(format!("tui-lab-sess-{}", &id[id.len().saturating_sub(8)..]))
            .spawn(move || {
                let mut session = session;
                for mail in rx {
                    match mail {
                        Mail::Job(job) => job(&mut session),
                        Mail::Shutdown => break,
                    }
                }
                // Draining shutdown: stop the child so nothing outlives the
                // actor.
                session.stop().ok();
            })
            .expect("spawn session actor");
        SessionActor {
            id,
            tx,
            join: Mutex::new(Some(join)),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send a closure to run against the session and await its reply. The
    /// closure completes on the actor thread; this future resolves when the
    /// closure has finished (the reply is delivered through the caller's
    /// own oneshot inside the closure).
    pub async fn send<R, F>(&self, job: F) -> Result<R, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> R + Send + 'static,
    {
        let (rtx, rrx) = tokio::sync::oneshot::channel::<R>();
        let job: Job = Box::new(move |session: &mut Session| {
            let _ = rtx.send(job(session));
        });
        self.tx
            .send(Mail::Job(job))
            .map_err(|_| ActorError::ActorGone(self.id.clone()))?;
        rrx.await.map_err(|_| ActorError::ActorDropped(self.id.clone()))
    }

    /// Try to send without awaiting (used on the actor thread itself and by
    /// stop paths that must not block).
    pub fn send_sync<R, F>(&self, job: F) -> Result<tokio::sync::oneshot::Receiver<R>, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> R + Send + 'static,
    {
        let (rtx, rrx) = tokio::sync::oneshot::channel::<R>();
        let job: Job = Box::new(move |session: &mut Session| {
            let _ = rtx.send(job(session));
        });
        self.tx
            .send(Mail::Job(job))
            .map_err(|_| ActorError::ActorGone(self.id.clone()))?;
        Ok(rrx)
    }

    /// Ask the actor to stop the session and exit. Idempotent per handle.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Mail::Shutdown);
        if let Ok(mut j) = self.join.lock() {
            if let Some(handle) = j.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Why an actor call failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ActorError {
    #[error("session actor '{0}' is gone (stopped or crashed)")]
    ActorGone(String),
    #[error("session actor '{0}' dropped its reply")]
    ActorDropped(String),
}

impl ActorError {
    /// Map to the MCP error category for a missing/unresponsive session.
    pub fn category(&self) -> crate::error::ErrorCategory {
        crate::error::ErrorCategory::NoSession
    }
}

/// The session pool: the actor-backed replacement for the global
/// `Mutex<SessionManager>` on the MCP surface.
pub struct SessionPool {
    directory: RwLock<HashMap<String, SessionActor>>,
    active: Mutex<Option<String>>,
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPool {
    pub fn new() -> Self {
        SessionPool {
            directory: RwLock::new(HashMap::new()),
            active: Mutex::new(None),
        }
    }

    /// Create + start a session from a full spec; the actor owns it from
    /// birth. Returns the session id (and the start outcome computed inside
    /// the actor, so launch errors surface verbatim).
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        backend: &str,
        isolation: &str,
    ) -> Result<String, anyhow::Error> {
        let id = format!("sess-{}", uuid::Uuid::new_v4().simple());
        let mut s = Session::new(id.clone(), command.to_string());
        let spec = LaunchSpec {
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
        self.directory
            .write()
            .expect("session directory")
            .insert(id.clone(), SessionActor::spawn(s));
        *self.active.lock().expect("active pointer") = Some(id.clone());
        Ok(id)
    }

    /// Run a closure against a session (explicit id or the active one).
    pub async fn with_session<R, F>(
        &self,
        id: Option<&str>,
        job: F,
    ) -> Result<R, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> R + Send + 'static,
    {
        let actor = self.resolve(id).ok_or_else(|| {
            ActorError::ActorGone(id.map(str::to_string).unwrap_or_else(|| "none".into()))
        })?;
        actor.send(job).await
    }

    /// The actor handle for one session (explicit id or active).
    pub fn resolve(&self, id: Option<&str>) -> Option<SessionActor> {
        let wanted = match id {
            Some(i) => i.to_string(),
            None => self.active.lock().ok()?.clone()?,
        };
        self.directory
            .read()
            .ok()
            .and_then(|d| d.get(&wanted).cloned())
    }

    /// Whether a session exists.
    pub fn contains(&self, id: &str) -> bool {
        self.directory
            .read()
            .map(|d| d.contains_key(id))
            .unwrap_or(false)
    }

    pub fn active_id(&self) -> Option<String> {
        self.active.lock().ok()?.clone()
    }

    /// Every session id, sorted for stable output.
    pub fn list(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .directory
            .read()
            .map(|d| d.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Restart a session inside its own actor: same id, next generation.
    /// Returns `(id, generation)` or the launch error.
    pub async fn restart(&self, id: &str) -> Result<(String, u32), anyhow::Error> {
        let actor = self
            .resolve(Some(id))
            .ok_or_else(|| anyhow::anyhow!("no session '{}'", id))?;
        let owned = id.to_string();
        actor
            .send(move |s: &mut Session| {
                s.restart()?;
                Ok((owned, s.generation))
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
    }

    /// Stop a session and remove its actor. Idempotent: stopping an unknown
    /// id is success (the old manager's contract — "already gone").
    pub async fn stop(&self, id: &str) -> anyhow::Result<()> {
        let Some(actor) = self.resolve(Some(id)) else {
            return Ok(());
        };
        // Stop synchronously inside the actor FIRST (flushes backend state),
        // then shut the actor down and drop it from the directory.
        let _ = actor.send(|s: &mut Session| s.stop()).await;
        actor.shutdown();
        if let Ok(mut d) = self.directory.write() {
            d.remove(id);
        }
        if self.active_id().as_deref() == Some(id) {
            if let Ok(mut a) = self.active.lock() {
                *a = None;
            }
        }
        Ok(())
    }

    /// Stop every session and tear down the actors (run close
    /// kill_sessions=true path).
    pub async fn stop_all(&self) -> Vec<String> {
        let ids = self.list();
        let mut stopped = Vec::new();
        for id in ids {
            if self.stop(&id).await.is_ok() {
                stopped.push(id);
            }
        }
        stopped
    }

    /// Drain one session's terminal events (run close flush path).
    pub async fn drain_events(
        &self,
        id: &str,
    ) -> Result<Vec<crate::events::TerminalEvent>, ActorError> {
        self.with_session(Some(id), |s| s.drain_events()).await
    }
}

impl Drop for SessionPool {
    fn drop(&mut self) {
        if let Ok(d) = self.directory.read() {
            for actor in d.values() {
                let _ = actor.tx.send(Mail::Shutdown);
            }
        }
        // Do not join here: the pool can drop inside an async runtime where
        // joining would block. Actor threads exit on their mailbox closing.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline property: two sessions proceed in PARALLEL. The old
    /// global Mutex serialized them — a slow observe on A delayed B by the
    /// full wait. Here A sits in a 600 ms actor job while B completes its
    /// own job; B's wall time proves no cross-blocking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_do_not_block_each_other() {
        let pool = std::sync::Arc::new(SessionPool::new());
        let a = pool
            .start("python3", &["-c".into(), "print('A'); input()".into()], None, &[], 80, 24, "auto", "local")
            .await
            .expect("start A");
        let b = pool
            .start("python3", &["-c".into(), "print('B'); input()".into()], None, &[], 80, 24, "auto", "local")
            .await
            .expect("start B");

        let pool_a = pool.clone();
        let a2 = a.clone();
        let slow = tokio::spawn(async move {
            pool_a
                .with_session(Some(&a2), |_s: &mut Session| {
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    42u32
                })
                .await
                .expect("slow job")
        });
        // Give the slow job a moment to enter the actor.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let start = std::time::Instant::now();
        let fast = pool
            .with_session(Some(&b), |s: &mut Session| s.id.clone())
            .await
            .expect("fast job");
        let elapsed = start.elapsed();
        let _ = slow.await.expect("slow completes");
        assert_eq!(fast, b);
        assert!(
            elapsed < std::time::Duration::from_millis(400),
            "B's job must not wait for A's 600 ms job (took {elapsed:?})"
        );
        pool.stop(&a).await.ok();
        pool.stop(&b).await.ok();
    }

    #[tokio::test]
    async fn start_observe_stop_roundtrip() {
        let pool = SessionPool::new();
        let id = pool
            .start("python3", &["-c".into(), "print('actor-ok'); input()".into()], None, &[], 80, 24, "auto", "local")
            .await
            .expect("start");
        let text = pool
            .with_session(Some(&id), |s: &mut Session| {
                let scr = s.observe(80).expect("observe");
                scr.viewport_text.join("\n")
            })
            .await
            .expect("job");
        assert!(text.contains("actor-ok"), "{text}");
        pool.stop(&id).await.expect("stop");
        assert!(!pool.contains(&id));
    }

    #[tokio::test]
    async fn unknown_session_is_actor_gone() {
        let pool = SessionPool::new();
        let err = pool
            .with_session(Some("sess-nope"), |_s: &mut Session| ())
            .await
            .expect_err("no such session");
        assert!(matches!(err, ActorError::ActorGone(_)));
        assert_eq!(err.category(), crate::error::ErrorCategory::NoSession);
    }
}
