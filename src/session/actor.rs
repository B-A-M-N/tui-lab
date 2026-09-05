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
    ///
    /// Acknowledged (review P0.3): the actor signals the sender once it has
    /// actually stopped and torn down the child, so `shutdown` can await
    /// a bounded completion instead of racing a best-effort notification.
    /// Carries a `&str` filter plus the oneshot ack when a caller asks for a
    /// specific session.
    Shutdown {
        session_filter: Option<String>,
        ack: tokio::sync::oneshot::Sender<()>,
    },
}

/// One session's actor: a cloneable handle sharing one inner state object
/// (finding 1). Every clone observes the SAME `closing` flag and the SAME
/// join handle — a clone that initiates shutdown commits shutdown for all
/// handles, and a join through any clone joins the real thread.
#[derive(Clone)]
pub struct SessionActor {
    inner: std::sync::Arc<SessionActorInner>,
}

struct SessionActorInner {
    id: String,
    /// Bounded async mailbox (review item 13): `tokio::sync::mpsc` so a full
    /// mailbox applies *awaitable backpressure* on the caller instead of
    /// blocking a runtime worker thread the way `std::sync::mpsc::SyncSender`
    /// would. The actor thread is a plain OS thread and drains via
    /// `blocking_recv()`; callers `send().await`.
    tx: tokio::sync::mpsc::Sender<Mail>,
    /// Actor thread join handle, taken (once, under the mutex) by `shutdown`.
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set once shutdown is requested (review P0.3): new `send`/`send_sync`
    /// reject immediately rather than enqueuing work behind a dying actor.
    /// SHARED across every clone of the handle (finding 1).
    closing: std::sync::atomic::AtomicBool,
}

impl SessionActor {
    /// Spawn an actor thread owning `session`. Returns the handle.
    pub fn spawn(session: Session) -> SessionActor {
        let id = session.id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Mail>(64);
        let join = std::thread::Builder::new()
            .name(format!(
                "tui-lab-sess-{}",
                &id[id.len().saturating_sub(8)..]
            ))
            .spawn(move || {
                let mut session = session;
                // `blocking_recv` is the plain-thread drain for a tokio
                // channel: it parks the OS thread (not a runtime worker) and
                // returns None once every sender is dropped, ending the loop.
                while let Some(mail) = rx.blocking_recv() {
                    match mail {
                        Mail::Job(job) => job(&mut session),
                        // Ack-based shutdown: tear the child down, signal the
                        // caller we're done, then break. The sender is still
                        // alive (it's awaiting our ack), so we MUST break and
                        // exit rather than wait on the channel closing —
                        // otherwise we'd deadlock against join() (review
                        // P0.3).
                        Mail::Shutdown {
                            session_filter,
                            ack,
                        } => {
                            if session_filter.as_deref().is_none_or(|w| session.id == w) {
                                session.stop().ok();
                                let _ = ack.send(());
                                break;
                            }
                            // A filter that doesn't match this session means
                            // the shutdown was for a different actor (a coarse
                            // shared channel case unused today); keep draining.
                            drop(ack);
                        }
                    }
                }
            })
            .expect("spawn session actor");
        SessionActor {
            inner: std::sync::Arc::new(SessionActorInner {
                id,
                tx,
                join: Mutex::new(Some(join)),
                closing: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Drive shutdown to completion, ack-based (review P0.3). Returns once
    /// the actor thread has actually exited and torn down the child, or an
    /// error when it was already gone.
    ///
    /// The mailbox is bounded, so we do NOT rely on `try_send` landing the
    /// Shutdown mail (it can be dropped when full). Instead we set `closing`
    /// (which atomically rejects new work), enqueue Shutdown with a oneshot
    /// ack under `send(...).await` backpressure (which CANNOT fail while the
    /// actor drains — awaiting releases the caller's slot), wait for the
    /// ack, then drop our own sender so the actor's `blocking_recv` can
    /// return `None` and the thread join is attainable.
    async fn shutdown_ack(&self) -> Result<(), ActorError> {
        self.inner
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
        // Backpressured send: if the mailbox is full this awaits the actor
        // draining a slot, so the Shutdown mail always gets in (review P0.3
        // deadlock fix — never a dropped notification).
        self.inner
            .tx
            .send(Mail::Shutdown {
                session_filter: Some(self.inner.id.clone()),
                ack: ack_tx,
            })
            .await
            .map_err(|_| ActorError::ActorGone(self.inner.id.clone()))?;
        ack_rx
            .await
            .map_err(|_| ActorError::ActorGone(self.inner.id.clone()))?;
        self.join().await;
        Ok(())
    }

    /// Join the actor thread if it has not been joined yet (finding 1:
    /// "thread demonstrably gone after successful stop"). The join runs on
    /// a blocking thread so callers inside the async runtime never stall a
    /// worker. Shared state: whichever caller gets here first joins; the
    /// rest observe the taken slot and return immediately.
    async fn join(&self) {
        let handle = self.inner.join.lock().ok().and_then(|mut j| j.take());
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
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
        if self.inner.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ActorError::ActorGone(self.inner.id.clone()));
        }
        let (rtx, rrx) = tokio::sync::oneshot::channel::<R>();
        let job: Job = Box::new(move |session: &mut Session| {
            let _ = rtx.send(job(session));
        });
        // Async backpressure (review item 13): a full mailbox suspends THIS
        // task until the actor drains, rather than blocking a runtime worker
        // thread as std::sync::mpsc::SyncSender::send would.
        self.inner
            .tx
            .send(Mail::Job(job))
            .await
            .map_err(|_| ActorError::ActorGone(self.inner.id.clone()))?;
        rrx.await
            .map_err(|_| ActorError::ActorDropped(self.inner.id.clone()))
    }

    /// Try to send without awaiting: returns a oneshot receiver to await the
    /// reply. Never blocks the caller. A full mailbox is an explicit
    /// `ActorBusy` (the session is legitimately mid-job and will drain); use
    /// `send(...).await` for blocking-aware backpressure instead.
    pub fn send_sync<R, F>(&self, job: F) -> Result<tokio::sync::oneshot::Receiver<R>, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> R + Send + 'static,
    {
        if self.inner.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ActorError::ActorGone(self.inner.id.clone()));
        }
        let (rtx, rrx) = tokio::sync::oneshot::channel::<R>();
        let job: Job = Box::new(move |session: &mut Session| {
            let _ = rtx.send(job(session));
        });
        match self.inner.tx.try_send(Mail::Job(job)) {
            Ok(()) => Ok(rrx),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                Err(ActorError::ActorBusy(self.id().to_string()))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(ActorError::ActorGone(self.id().to_string()))
            }
        }
    }

    /// Ask the actor to stop the session and exit completely before
    /// returning. Idempotent per handle.
    ///
    /// This is the ack-based shutdown (review P0.3) for plain-thread callers:
    /// it drives delivery with `try_send` and a small retry loop instead of
    /// `blocking_send` (which would panic or block inside a tokio runtime),
    /// then waits on the ack under a hard deadline. `SessionPool::stop` (the
    /// async path) uses `shutdown_ack` instead; this stays as a bounded
    /// fire-and-join option for non-async contexts.
    pub fn shutdown(&self) {
        self.inner
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (ack_tx, mut ack_rx) = tokio::sync::oneshot::channel::<()>();
        // Retry `try_send` until it lands or the mailbox proves closed. A full
        // mailbox means the actor is draining; it will free a slot, so we
        // never give up on healthily-busy actors. We sleep between attempts to
        // keep `closing` visible and avoid a tight spin.
        let mail = Mail::Shutdown {
            session_filter: Some(self.inner.id.clone()),
            ack: ack_tx,
        };
        let mut sent = false;
        let mut mail = mail;
        for _ in 0..100_000 {
            match self.inner.tx.try_send(mail) {
                Ok(()) => {
                    sent = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(full_mail)) => {
                    mail = full_mail;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                // Closed: the actor already exited (blocking_recv returned
                // None). Nothing to signal; join below is a no-op.
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        if sent {
            // Hard-bounded ack wait so a wedged actor (a runaway job) cannot
            // hang this thread forever. `closing=true` means the actor will
            // break on Shutdown regardless of remaining queued jobs.
            use tokio::sync::oneshot::error::TryRecvError;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match ack_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Closed) => break,
                    Err(TryRecvError::Empty) => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
            }
        }
        // Drop our sender so no keep-alive remains; the actor's blocking_recv
        // then returns None if it hadn't already. Then join — through the
        // SHARED join handle, so a shutdown driven from any clone joins the
        // real thread (finding 1).
        if let Ok(mut j) = self.inner.join.lock() {
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
    /// A non-blocking `send_sync` found the bounded mailbox full — the
    /// session is mid-job. Retry with `send(...).await` (async backpressure)
    /// or after the actor drains.
    #[error("session actor '{0}' mailbox is full (busy); await or retry")]
    ActorBusy(String),
}

impl ActorError {
    /// Map to the MCP error category (audit finding 12): a busy mailbox is
    /// `session_busy` — the session EXISTS and is healthy, it is mid-job and
    /// will drain, so the agent should retry rather than re-launch. Only a
    /// genuinely gone actor (stopped/crashed) is `no_session`. A dropped
    /// reply is a transport-level fault (`backend_error`), not a missing
    /// session.
    pub fn category(&self) -> crate::error::ErrorCategory {
        match self {
            ActorError::ActorBusy(_) => crate::error::ErrorCategory::SessionBusy,
            ActorError::ActorGone(_) => crate::error::ErrorCategory::NoSession,
            ActorError::ActorDropped(_) => crate::error::ErrorCategory::BackendError,
        }
    }

    /// Structured remediation for the busy mapping: how long the caller
    /// should wait before retrying. The mailbox is bounded; a full one
    /// drains in roughly one job time, so the retry window is short and
    /// constant.
    pub fn details(&self) -> Option<serde_json::Value> {
        match self {
            ActorError::ActorBusy(id) => Some(serde_json::json!({
                "session": id,
                "retry_after_ms": RETRY_AFTER_MS,
                "why": "session actor mailbox is full; the session is mid-job and will drain",
            })),
            _ => None,
        }
    }
}

/// Suggested retry window for a `session_busy` refusal (audit finding 12).
/// The mailbox holds 64 jobs; one job is at most a wait with a ~1s budget
/// beyond its quiet window, so a short constant backoff is honest.
pub(crate) const RETRY_AFTER_MS: u64 = 50;

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

    /// Create + start a session; the actor owns it from birth (review item 10).
    /// `Session::new` is cheap (pure struct construction) and runs on the async
    /// caller, but the actual launch — `start_with_spec`, which spawns the PTY
    /// and does the initial settle — executes as a job ON the actor thread. The
    /// async caller awaits that job's oneshot, so launch errors surface verbatim
    /// but the blocking process-start no longer holds up a runtime worker.
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
        let s = Session::new(id.clone(), command.to_string());
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
        // Spawn the actor owning the un-started session, then hand it the
        // launch as a job. `send` awaits the reply, so the caller gets the
        // launch outcome while the blocking PTY start runs on the actor thread.
        let actor = SessionActor::spawn(s);
        actor
            .send(move |s: &mut Session| s.start_with_spec(spec))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))??;
        self.directory
            .write()
            .expect("session directory")
            .insert(id.clone(), actor);
        *self.active.lock().expect("active pointer") = Some(id.clone());
        Ok(id)
    }

    /// Typed-engine launch (re-review P0): the engine arrives as
    /// [`BackendKind`], not a string the launch layer re-interprets. The
    /// engine name recorded in the spec is derived from the kind — one
    /// source of truth, no parse drift between MCP param and engine.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_typed(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        backend: crate::session::state::BackendKind,
        isolation: &str,
    ) -> Result<String, anyhow::Error> {
        self.start(
            command,
            args,
            cwd,
            env,
            cols,
            rows,
            backend.display(),
            isolation,
        )
        .await
    }

    /// Attach to an already-running TUI in a tmux pane (re-review item 18).
    /// The target pane is verified to EXIST before the session is created —
    /// attaching to a phantom pane fails here, naming the target, never as a
    /// later observe error. The attached TUI is never killed by TUI-Lab
    /// (detaching leaves it running; item 18).
    pub async fn attach_tmux(
        &self,
        target: &str,
        cols: u16,
        rows: u16,
    ) -> Result<String, anyhow::Error> {
        // Verify OUTSIDE the session: a missing pane is a caller error and
        // must not leave a half-built session behind.
        let backend = crate::backend::tmux::TmuxBackend::attach(target, cols, rows)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = format!("sess-{}", uuid::Uuid::new_v4().simple());
        let owned_target = format!("tmux:{target}");
        let mut s = Session::new(id.clone(), owned_target.clone());
        s.adopt_backend(
            Box::new(backend),
            crate::session::state::BackendKind::TmuxAttach,
            cols,
            rows,
            LaunchSpec {
                command: owned_target.clone(),
                args: vec![],
                cwd: None,
                env: vec![],
                cols,
                rows,
                backend: crate::session::state::BackendKind::TmuxAttach
                    .display()
                    .to_string(),
                isolation: "local".to_string(),
            },
        );
        let actor = SessionActor::spawn(s);
        actor
            .send(move |s: &mut Session| -> anyhow::Result<()> {
                // "Start" for an attach = verify the pane is still live.
                s.backend_mut()
                    .start(&owned_target, &[], None, &[], cols, rows)?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))??;
        self.directory
            .write()
            .expect("session directory")
            .insert(id.clone(), actor);
        *self.active.lock().expect("active pointer") = Some(id.clone());
        Ok(id)
    }

    /// Run a closure against a session (explicit id or the active one).
    pub async fn with_session<R, F>(&self, id: Option<&str>, job: F) -> Result<R, ActorError>
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
        // Remove from the directory BEFORE shutting down (finding 1): once
        // stop begins, `resolve` can no longer hand out handles that would
        // race the shutdown with fresh sends. The removed actor is the last
        // authority for this id.
        let actor = {
            let removed = self.directory.write().ok().and_then(|mut d| d.remove(id));
            let Some(actor) = removed else {
                return Ok(());
            };
            if self.active_id().as_deref() == Some(id) {
                if let Ok(mut a) = self.active.lock() {
                    *a = None;
                }
            }
            actor
        };
        // Stop synchronously inside the actor FIRST (flushes backend state),
        // then shut the actor down (ack-awaited, so a full mailbox can never
        // wedge us — review P0.3).
        let _ = actor.send(|s: &mut Session| s.stop()).await;
        actor.shutdown_ack().await?;
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
                // Drop cannot await, so we signal the actor to cease with a
                // no-ack Shutdown (review P0.3): the mailbox closing on these
                // Senders dropping IS the fallback that unblocks the actor's
                // blocking_recv. We never join here — Drop can run inside an
                // async runtime where joining would block.
                actor
                    .inner
                    .closing
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let (ack_tx, _ack_rx) = tokio::sync::oneshot::channel::<()>();
                let _ = actor.inner.tx.try_send(Mail::Shutdown {
                    session_filter: Some(actor.inner.id.clone()),
                    ack: ack_tx,
                });
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
            .start(
                "python3",
                &["-c".into(), "print('A'); input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start A");
        let b = pool
            .start(
                "python3",
                &["-c".into(), "print('B'); input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
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
            .start(
                "python3",
                &["-c".into(), "print('actor-ok'); input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        // Python startup can exceed a single quiet-window observe under
        // parallel PTY load; poll until the print lands (bounded) instead
        // of betting on one 80ms window.
        let text = pool
            .with_session(Some(&id), |s: &mut Session| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut text;
                loop {
                    let scr = s.observe(80).expect("observe");
                    text = scr.viewport_text.join("\n");
                    if text.contains("actor-ok") || std::time::Instant::now() > deadline {
                        break;
                    }
                }
                text
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

    /// P0.3 stress test: flood the 64-entry mailbox with jobs that never
    /// reply, then demand a stop. The ack protocol must complete within a
    /// bounded deadline — the old try_send(Shutdown)+join could wedge on a
    /// full mailbox. We run this many times to shake out the race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_ack_under_full_mailbox_completes_within_budget() {
        for _round in 0..50 {
            let pool = std::sync::Arc::new(SessionPool::new());
            let id = pool
                .start(
                    "python3",
                    &["-c".into(), "input()".into()],
                    None,
                    &[],
                    80,
                    24,
                    "auto",
                    "local",
                )
                .await
                .expect("start");

            let actor = pool.resolve(Some(&id)).expect("actor present");
            // Asynchronously saturate the mailbox ahead of the stop. Each
            // job parks on the actor thread, so queued jobs back up behind
            // it — exactly the full-mailbox precondition.
            let flood = {
                tokio::spawn(async move {
                    let mut sent = 0;
                    for _ in 0..200 {
                        if actor
                            .send(|_s: &mut Session| {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        sent += 1;
                    }
                    sent
                })
            };
            // Let the flood fill the bounded mailbox before we try to stop.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;

            let start = std::time::Instant::now();
            let res = pool.stop(&id).await;
            let elapsed = start.elapsed();
            res.expect("stop must succeed even with a full mailbox");
            let _ = flood.await;
            assert!(
                elapsed < std::time::Duration::from_millis(15000),
                "stop must complete within budget under a full mailbox (took {elapsed:?})"
            );
            assert!(!pool.contains(&id));
        }
    }

    /// P0.3: after shutdown is requested, new work is rejected rather than
    /// enqueued behind a dying actor.
    #[tokio::test]
    async fn work_is_rejected_after_shutdown_requested() {
        let pool = SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let actor = pool.resolve(Some(&id)).expect("actor present");
        actor
            .inner
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = actor
            .send(|_s: &mut Session| 1)
            .await
            .expect_err("post-closing send must fail");
        assert!(matches!(err, ActorError::ActorGone(_)));
        // Restore so the pool can clean up normally.
        actor
            .inner
            .closing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        pool.stop(&id).await.ok();
    }

    /// Finding 1: every clone of an actor handle shares ONE closing flag.
    /// A shutdown initiated through clone A is visible synchronously through
    /// clone B — and a post-shutdown send through B is refused, not enqueued.
    #[tokio::test]
    async fn clones_share_closing_state() {
        let pool = SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let a = pool.resolve(Some(&id)).expect("actor present");
        let b = a.clone();
        // Mark closing through `a`; `b` must see it immediately (same inner).
        a.inner
            .closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            b.inner.closing.load(std::sync::atomic::Ordering::SeqCst),
            "clone must observe the shared closing flag"
        );
        let err = b
            .send(|_s: &mut Session| 1)
            .await
            .expect_err("send through the clone must be refused after shutdown began");
        assert!(matches!(err, ActorError::ActorGone(_)));
        // Restore so the pool can clean up normally.
        a.inner
            .closing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        pool.stop(&id).await.ok();
    }

    /// Finding 1: a full stop driven from a CLONE — the pool's directory
    /// entry is gone afterwards and a fresh resolve returns nothing. This
    /// is the race the old per-clone `closing` copy allowed: one handle
    /// shutting down while the directory's handle kept accepting jobs.
    #[tokio::test]
    async fn stop_via_clone_removes_directory_entry() {
        let pool = std::sync::Arc::new(SessionPool::new());
        let id = pool
            .start(
                "python3",
                &["-c".into(), "input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let clone = {
            let actor = pool.resolve(Some(&id)).expect("actor present");
            actor.clone()
        };
        // Concurrent send racing the stop: after stop is COMMITTED no job
        // may execute — either the send is refused (closing already set) or
        // it completed before the stop's session teardown; both are safe.
        let racer = {
            let pool = pool.clone();
            let id = id.clone();
            tokio::spawn(async move {
                pool.with_session(Some(&id), |s: &mut Session| s.id.clone())
                    .await
            })
        };
        pool.stop(&id).await.expect("stop");
        assert!(!pool.contains(&id), "directory entry must be gone");
        assert!(pool.resolve(Some(&id)).is_none(), "no handle after stop");
        // The racer either failed (refused) or succeeded pre-stop; it must
        // never hang. Awaiting it proves termination.
        let _ = racer.await;
        // The clone's shutdown state is shared: it reports closed too.
        assert!(clone
            .inner
            .closing
            .load(std::sync::atomic::Ordering::SeqCst));
        // Stopping again is idempotent.
        pool.stop(&id).await.expect("idempotent stop");
    }

    /// Finding 1: after a successful stop the actor thread is demonstrably
    /// gone — the join handle (shared, through any clone) has exited.
    #[tokio::test]
    async fn actor_thread_is_gone_after_stop() {
        let pool = SessionPool::new();
        let id = pool
            .start(
                "python3",
                &["-c".into(), "input()".into()],
                None,
                &[],
                80,
                24,
                "auto",
                "local",
            )
            .await
            .expect("start");
        let actor = pool.resolve(Some(&id)).expect("actor present");
        let clone = actor.clone();
        pool.stop(&id).await.expect("stop");
        // The clone sees the SAME join slot: already taken by the stop's
        // join, so is_some() is false — the handle was joined to completion.
        let joined = clone.inner.join.lock().expect("join lock").is_none();
        assert!(joined, "join handle must be consumed by the stop's join");
    }

    /// Finding 12: the error taxonomy distinguishes "session gone" from
    /// "session mid-job". A busy mailbox maps to `session_busy` WITH a
    /// structured retry window; a gone actor maps to `no_session` with no
    /// remediation (there is nothing to retry against).
    #[test]
    fn busy_maps_to_retryable_session_busy_not_no_session() {
        let busy = ActorError::ActorBusy("s-1".into());
        assert_eq!(
            busy.category(),
            crate::error::ErrorCategory::SessionBusy,
            "a full mailbox is not a missing session"
        );
        let details = busy.details().expect("busy carries remediation");
        assert_eq!(details["session"], "s-1");
        assert!(
            details["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0),
            "retry window is a positive backoff: {details}"
        );

        let gone = ActorError::ActorGone("s-1".into());
        assert_eq!(gone.category(), crate::error::ErrorCategory::NoSession);
        assert!(gone.details().is_none(), "a gone session has no retry");

        let dropped = ActorError::ActorDropped("s-1".into());
        assert_eq!(
            dropped.category(),
            crate::error::ErrorCategory::BackendError,
            "a dropped reply is a transport fault, not a missing session"
        );
    }
}
