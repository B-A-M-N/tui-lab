//! tui_session: lifecycle, lease, backend/engine selection.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, ok};
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
    match action {
        A::Start => {
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
            let facts = s
                .with_sess(Some(&id), |s| {
                    let caps = s.capabilities();
                    let launch = s.launch().cloned();
                    let version = s.backend_version();
                    let kind = s.backend_kind;
                    let generation = s.generation;
                    let iso_evidence = s.isolation_evidence().cloned();
                    (caps, launch, version, kind, generation, iso_evidence)
                })
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
            let run_id = s.run.lock().unwrap().id.clone();
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
            let facts = s
                .with_sess(Some(&id), |s| {
                    let caps = s.capabilities();
                    let version = s.backend_version();
                    let kind = s.backend_kind;
                    let generation = s.generation;
                    let process = s.process();
                    (caps, version, kind, generation, process)
                })
                .await;
            let (caps, version, kind, generation, process) = match facts {
                Ok(f) => f,
                Err(e) => return e,
            };
            if let Some(spec) = s
                .with_sess(Some(&id), |s| s.launch().cloned())
                .await
                .ok()
                .flatten()
            {
                s.run.lock().unwrap().set_launch_spec(&id, spec);
            }
            let run_id = s.run.lock().unwrap().id.clone();
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
            match s.sessions.stop(&id).await {
                Ok(()) => {
                    // Session gone: drop its run ownership too.
                    s.session_owners
                        .lock()
                        .expect("session owner lock")
                        .remove(&id);
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
                            "note": "machine-driving tools (act/explore/audit/replay) refuse this session while the lease is valid; observe stays allowed",
                        })),
                        Err(existing) => ok(json!({
                            "session": id,
                            "leased": false,
                            "holder": existing.holder,
                            "remaining_ms": existing.remaining_ms(),
                            "note": "a live lease is already held; release it or wait for expiry",
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
            let selector = id.clone();
            s.with_sess(Some(&selector), move |s| {
                let released = s.release_lease();
                ok(json!({ "session": id.clone(), "released": released }))
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
