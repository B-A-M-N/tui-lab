//! tui_session: lifecycle, lease, backend/engine selection.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, err_with_details, ok};
use crate::mcp::params::*;
use rmcp::serde_json::json;

/// Body of `tui_session` (Phase 5 extraction): the #[tool] method in
/// `super` decodes params and delegates here. `s` is the server,
/// whose private fields this child module can see unchanged.
pub(crate) async fn tui_session(
    s: &crate::mcp::tools::TuiLabServer,
    p: rmcp::handler::server::wrapper::Parameters<TuiSessionParams>,
) -> rmcp::model::CallToolResult {
    let p = p.0;
    use crate::mcp::params::SessionAction as A;
    let Some(action) = p.action.known() else {
        return err(
            ErrorCategory::InvalidRequest,
            format!(
                "unknown session action '{}' (expected one of: {})",
                match &p.action {
                    crate::mcp::params::Known::Other(s) => s.clone(),
                    _ => String::new(),
                },
                <A as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
            ),
        );
    };
    // Action-scoped field validation (P1-20): meaningless fields must
    // not silently ride a request, and attach-only selectors must not
    // appear on spawn.
    if p.target.is_some() && *action != A::Attach {
        return err(
            ErrorCategory::InvalidRequest,
            "target is attach-only; use tui_session action=attach target=...",
        );
    }
    if p.attach_backend.is_some() && *action != A::Attach {
        return err(
            ErrorCategory::InvalidRequest,
            "attach_backend is attach-only; use tui_session action=attach attach_backend=tmux",
        );
    }
    if *action == A::Attach && (p.command.is_some() || p.args.is_some()) {
        return err(
            ErrorCategory::InvalidRequest,
            "command/args are spawn-only; action=attach adopts an existing pane via target",
        );
    }
    match action {
        A::Start => {
            // Beta-audit P0.1: the shared lifecycle lease spans the
            // WHOLE launch — actor start, owner binding, fact readback,
            // launch-spec attach. The old shape bound ownership AFTER
            // the async launch completed, so a run switch landing in
            // that window bound the session to whichever run was
            // current at completion, not the one that authorized it.
            let _lease = s.lifecycle.shared("session-start").await;
            let cols = p.cols.unwrap_or(80);
            let rows = p.rows.unwrap_or(24);
            let command = match &p.command {
                Some(c) => c.clone(),
                None => return err(ErrorCategory::InvalidRequest, "start requires 'command'"),
            };
            // Honest backend/isolation negotiation (spec section 34). The
            // engine selector is TYPED (re-review P0): `Known<BackendParam>`
            // gives a closed schema enum; an unknown name is answered here
            // with the accepted list instead of a string match deeper in.
            // Resolve the typed engine for the session layer; `auto`
            // defaults to the portable PTY engine.
            let backend_kind = match p.backend.as_ref() {
                None => crate::session::state::BackendKind::PortableVt,
                Some(Known::Known(b)) => b.to_kind(),
                Some(Known::Other(other)) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "unknown backend '{}' (supported: {})",
                            other,
                            crate::mcp::params::BackendParam::VARIANTS.join(", ")
                        ),
                    )
                }
            };
            // Wave G item 77: typed isolation profile; the enum converts
            // into the engine-level Isolation.
            let isolation_param = p
                .isolation
                .clone()
                .unwrap_or(crate::mcp::params::Known::Known(
                    crate::mcp::params::IsolationParam::Local,
                ));
            let isolation: crate::session::isolation::Isolation = match &isolation_param {
                crate::mcp::params::Known::Known(ip) => (*ip).into(),
                crate::mcp::params::Known::Other(other) => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "unknown isolation profile '{}' (supported: local, clean, strict)",
                            other
                        ),
                    )
                }
            };
            let env: Vec<(String, String)> = p.env.unwrap_or_default().into_iter().collect();
            let isolation_name = isolation.name().to_string();
            let args = p.args.unwrap_or_default();
            // Launch inside the pool; the actor owns the session from
            // birth (Wave G item 73). The engine is selected by the typed
            // enum, not re-matched from the string here.
            let started = s.sessions.start_typed(
                &command,
                &args,
                p.cwd.as_deref(),
                &env,
                cols,
                rows,
                backend_kind,
                &isolation_name,
            );
            let id = match started.await {
                Ok(id) => id,
                Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
            };
            s.bind_session_owner(&id);
            // Read back the launch facts inside the actor (the session
            // never crosses the boundary — only this summary does).
            // Dispatched under the SAME lease the launch holds (a
            // second shared acquire could deadlock against a queued
            // lifecycle transition).
            let facts = s
                .with_sess_leased(
                    Some(&id),
                    |s| {
                        let caps = s.capabilities();
                        let launch = s.launch().cloned();
                        let version = s.backend_version();
                        let kind = s.backend_kind;
                        let generation = s.generation;
                        let iso_evidence = s.isolation_evidence().cloned();
                        (caps, launch, version, kind, generation, iso_evidence)
                    },
                    &_lease,
                )
                .await;
            let (caps, launch, version, kind, generation, iso_evidence) = match facts {
                Ok(f) => f,
                Err(e) => return e,
            };
            // Attach the launch spec to the run, per session (audit
            // re-review item 1 + P0 fix 6: the run owns session/launch
            // correlation for *all* sessions, so relaunches, restarts,
            // and multi-session replay share one identity). Short lock,
            // after the actor reply — never across an await.
            if let Some(spec) = launch.clone() {
                s.run.lock().unwrap().set_launch_spec(&id, spec);
            }
            let run_id = s.run.lock().unwrap().id().to_string();
            ok(json!({
                "session": id,
                "generation": generation,
                "run": run_id,
                "backend": { "name": kind, "version": version },
                "capabilities": caps,
                "launch": launch,
                "isolation": iso_evidence,
            }))
        }
        // Re-review item 18: adopt an ALREADY-RUNNING TUI that lives in
        // a tmux pane. This is brownfield in the second sense: the
        // process predates TUI-Lab and belongs to the user. TUI-Lab
        // never kills the pane on detach.
        A::Attach => {
            // Beta-audit P0.1: same whole-operation lease as Start —
            // an attach authorized under run A binds to run A.
            let _lease = s.lifecycle.shared("session-attach").await;
            if let Some(known) = p.attach_backend.as_ref() {
                if known.known() != Some(&crate::mcp::params::AttachBackendParam::Tmux) {
                    return err(
                        ErrorCategory::InvalidRequest,
                        format!(
                            "unknown attach backend '{}' (supported: {})",
                            match known {
                                crate::mcp::params::Known::Other(o) => o.clone(),
                                _ => String::new(),
                            },
                            <crate::mcp::params::AttachBackendParam as crate::mcp::params::EnumVariants>::VARIANTS.join(", ")
                        ),
                    );
                }
            }
            let target = match p.target.as_deref() {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "attach requires 'target' (tmux session:window.pane, e.g. 'main:0.0')",
                    )
                }
            };
            let cols = p.cols.unwrap_or(80);
            let rows = p.rows.unwrap_or(24);
            let id = match s.sessions.attach_tmux(&target, cols, rows).await {
                Ok(id) => id,
                Err(e) => return err(ErrorCategory::BackendError, e.to_string()),
            };
            s.bind_session_owner(&id);
            // Readback under the attach's own lease (no nested acquire).
            let facts = s
                .with_sess_leased(
                    Some(&id),
                    |s| {
                        let caps = s.capabilities();
                        let version = s.backend_version();
                        let kind = s.backend_kind;
                        let generation = s.generation;
                        let process = s.process();
                        (caps, version, kind, generation, process)
                    },
                    &_lease,
                )
                .await;
            let (caps, version, kind, generation, process) = match facts {
                Ok(f) => f,
                Err(e) => return e,
            };
            if let Some(spec) = s
                .with_sess_leased(Some(&id), |s| s.launch().cloned(), &_lease)
                .await
                .ok()
                .flatten()
            {
                s.run.lock().unwrap().set_launch_spec(&id, spec);
            }
            let run_id = s.run.lock().unwrap().id().to_string();
            ok(json!({
                "session": id,
                "generation": generation,
                "run": run_id,
                "backend": { "name": kind, "version": version },
                "capabilities": caps,
                "attach": {
                    "target": target,
                    "engine": "tmux-attach",
                    "note": "the attached TUI is NOT killed on stop; use tui_session action=stop to detach only",
                },
                "process": process,
            }))
        }
        A::Restart => {
            let id = match p.id.clone().or_else(|| s.sessions.active_id()) {
                Some(i) => i,
                None => return err(ErrorCategory::NoSession, "no session to restart"),
            };
            // Audit P0-10 + beta-audit P0-4: authorize BEFORE mutating,
            // on the dedicated lifecycle path — the live lease FIRST
            // (resolved on the actor directly, so a closed run or a
            // foreign binding cannot mask it), then ownership. Restart
            // kills the child process; the old with_sess-based gate
            // discarded the Err for foreign/closed cases and let the
            // kill proceed.
            if s.run.lock().unwrap().is_closed() {
                let run_id = s.run.lock().unwrap().id().to_string();
                return err(
                    ErrorCategory::RunClosed,
                    format!(
                        "current run {run_id} is closed; resume it (tui_run action=resume) or start a fresh run before restarting sessions"
                    ),
                );
            }
            if let Err(refused) = s.authorize_lifecycle(&id, "restart").await {
                return refused;
            }
            // Restart the SAME logical session: same id, next generation,
            // reusing the stored LaunchSpec (spec section 13).
            match s.sessions.restart(&id).await {
                Ok((new_id, generation)) => {
                    let caps = s.with_sess(Some(&new_id), |s| s.capabilities()).await;
                    match caps {
                        Ok(caps) => ok(json!({
                            "session": new_id,
                            "generation": generation,
                            "restarted_from": id,
                            "capabilities": caps,
                        })),
                        Err(e) => e,
                    }
                }
                Err(e) => err(ErrorCategory::BackendError, e.to_string()),
            }
        }
        A::Stop => {
            let id = match p.id.clone().or_else(|| s.sessions.active_id()) {
                Some(i) => i,
                None => return err(ErrorCategory::NoSession, "no session"),
            };
            // Finding 4/13: stop kills the process the lease holder may be
            // driving — the lease gates it the same way it gates driving.
            // Beta-audit P0-4: authorization is the dedicated lifecycle
            // path (lease FIRST, then ownership), not with_sess — whose
            // closed-run/foreign guards run before any lease read and
            // whose Err the old code silently discarded, letting a leased
            // foreign session be killed.
            if let Err(refused) = s.authorize_lifecycle(&id, "stop").await {
                return refused;
            }
            match s.sessions.stop(&id).await {
                Ok(()) => {
                    // Session gone: drop its run ownership too.
                    s.session_owners
                        .lock()
                        .expect("session owner lock")
                        .unbind(&id);
                    ok(json!({ "stopped": id }))
                }
                Err(e) => err(ErrorCategory::BackendError, e.to_string()),
            }
        }
        A::List => ok(json!({ "sessions": s.sessions.list() })),
        // Wave G item 76: take the human control lease.
        A::Lease => {
            let id = match p.id.clone().or_else(|| s.sessions.active_id()) {
                Some(i) => i,
                None => return err(ErrorCategory::NoSession, "no session"),
            };
            let holder = p.holder.clone().unwrap_or_else(|| "human".into());
            let ttl = p.ttl_ms.unwrap_or(300_000);
            let selector = id.clone();
            s.with_sess(Some(&selector), move |s| {
                    match s.acquire_lease(&holder, ttl) {
                        Ok(lease) => ok(json!({
                            "session": id.clone(),
                            "leased": true,
                            "holder": lease.holder,
                            "ttl_ms": lease.ttl_ms,
                            // Finding 4: early release requires this token;
                            // expiry needs none.
                            "lease_id": lease.lease_id,
                            "note": "machine-driving tools (act/explore/audit/replay) refuse this session while the lease is valid; observe stays allowed; early release requires lease_id",
                        })),
                        Err(existing) => ok(json!({
                            "session": id,
                            "leased": false,
                            "holder": existing.holder,
                            "remaining_ms": existing.remaining_ms(),
                            "note": "a live lease is already held; release it (with its lease_id) or wait for expiry",
                        })),
                    }
                })
                .await
                .unwrap_or_else(|e| e)
        }
        A::Release => {
            let id = match p.id.clone().or_else(|| s.sessions.active_id()) {
                Some(i) => i,
                None => return err(ErrorCategory::NoSession, "no session"),
            };
            // Finding 4: releasing a live lease requires the lease_id the
            // acquire returned. Holder labels are shared ("human"); a
            // tokenless release must not drop a stranger's grant.
            let lease_id = match &p.lease_id {
                Some(t) => t.clone(),
                None => {
                    return err(
                        ErrorCategory::InvalidRequest,
                        "release requires 'lease_id' — the token the lease action returned (expiry needs no token)",
                    )
                }
            };
            let selector = id.clone();
            s.with_sess(Some(&selector), move |s| {
                match s.release_lease_with_token(&lease_id) {
                    Ok(released) => ok(json!({ "session": id.clone(), "released": released })),
                    Err(mismatch) => {
                        let live = s.active_lease();
                        err_with_details(
                            ErrorCategory::ControlLeased,
                            mismatch.to_string(),
                            json!({
                                "session": id,
                                "holder": live.as_ref().map(|l| l.holder.clone()),
                                "remaining_ms": live.as_ref().map(|l| l.remaining_ms()),
                            }),
                        )
                    }
                }
            })
            .await
            .unwrap_or_else(|e| e)
        }
        A::Status => {
            let id = match p.id.clone().or_else(|| s.sessions.active_id()) {
                Some(i) => i,
                None => return err(ErrorCategory::NoSession, "no session"),
            };
            let selector = id.clone();
            s.with_sess(Some(&selector), move |s| {
                let caps = s.capabilities();
                let lease = s.active_lease();
                let iso = s.isolation_evidence().cloned();
                ok(json!({
                    "session": id.clone(),
                    "command": s.command,
                    "backend": s.backend_kind,
                    "capabilities": caps,
                    "process": s.process(),
                    "lease": lease.map(|l| json!({
                        "holder": l.holder,
                        "remaining_ms": l.remaining_ms(),
                    })),
                    "isolation": iso,
                }))
            })
            .await
            .unwrap_or_else(|e| e)
        }
    }
}
