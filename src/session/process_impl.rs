//! Process lifecycle: launch/adopt/restart/stop, backend access, send/resize, scrollback/search.
//!
//! Impl-family extraction (Phase 2): the `Session` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. `Session` remains the single
//! per-session concurrency authority — no independent locking is
//! introduced. Signatures, visibility, and callers are unchanged.

use super::*;

impl Session {
    /// Get the terminal columns.
    pub fn cols(&self) -> u16 {
        self.launch.as_ref().map(|s| s.cols).unwrap_or(80)
    }

    /// Get the terminal rows.
    pub fn rows(&self) -> u16 {
        self.launch.as_ref().map(|s| s.rows).unwrap_or(24)
    }

    pub fn launch(&self) -> Option<&LaunchSpec> {
        self.launch.as_ref()
    }

    /// Start (or restart) the target program from a full spec (spec section 11).
    pub fn start_with_spec(&mut self, spec: LaunchSpec) -> anyhow::Result<()> {
        self.start_generation(spec, self.generation)
    }

    /// Adopt an EXTERNALLY-constructed backend (re-review item 18: the tmux
    /// attach engine is built by `TmuxBackend::attach`, which verifies the
    /// target BEFORE the session exists). Replaces the default engine and
    /// records the launch identity the session reports.
    pub fn adopt_backend(
        &mut self,
        backend: Box<dyn TerminalBackend>,
        kind: BackendKind,
        cols: u16,
        rows: u16,
        _spec: LaunchSpec,
    ) {
        self.backend = backend;
        self.backend_kind = kind;
        self.caps_at_start = self.backend.capabilities();
        // The session hook must ride the NEW backend (the default hook was
        // attached to the placeholder engine in `new`).
        self.backend.set_recording_hook(self.recording_slot.clone());
        let _ = (cols, rows);
    }

    /// Start the target as an EXPLICIT generation. `restart()` reserves the
    /// next generation BEFORE launching, so every generation-keyed artifact —
    /// scratch dirs, native channel reset, ProcessStarted, isolation
    /// evidence, run correlation — belongs to the generation that actually
    /// runs, not to the one being replaced (re-review P0: the old order
    /// launched generation N+1 under generation N's identity, then bumped).
    /// A failed launch leaves the old generation as history; nothing is
    /// silently re-used or rolled back.
    pub fn start_generation(&mut self, spec: LaunchSpec, generation: u32) -> anyhow::Result<()> {
        // Exact engine selection (re-review P0): compare parsed kinds, not
        // string classes — pipe vs portable was invisible to the boolean
        // test, so a requested pipe engine never got built on a fresh
        // session. First start always builds the requested engine.
        let requested = BackendKind::parse(&spec.backend)?;
        if requested != self.backend_kind {
            self.backend = requested.build(spec.cols, spec.rows)?;
            self.backend_kind = requested;
        }
        // Finding 49: install the terminal behavior contract before any
        // child traffic. A persona-supported portable backend changes query
        // responses here; environment declarations never substitute for it.
        if requested == BackendKind::PortableVt {
            let persona_id = spec
                .env
                .iter()
                .find(|(k, _)| k == "TUI_LAB_PERSONA")
                .map(|(_, v)| v.clone());
            if let Some(id) = persona_id {
                let persona = crate::terminal::TerminalPersona::builtin()
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown terminal persona '{id}'"))?;
                let any: &mut dyn std::any::Any = self.backend.as_any_mut();
                let portable = any
                    .downcast_mut::<crate::backend::PortablePtyBackend>()
                    .expect("PortableVt backend kind must downcast to PortablePtyBackend");
                portable.apply_terminal_persona(&persona);
            }
        }
        // THIS generation is now the live one — everything below keys on it.
        self.generation = generation;
        // Wave G item 77: apply the isolation profile. The declared string
        // is parsed here (invalid names fail loudly at launch, not silently
        // downgrade); the effective env is profile-filtered, strict wraps
        // the command in `unshare --net --` when the platform provides it,
        // and the backend is told to drop inherited env for clean/strict.
        let isolation = crate::session::isolation::Isolation::parse(&spec.isolation)
            .map_err(|e| anyhow::anyhow!(e))?;
        // Session-unique scratch HOME/TMPDIR keyed by (id, generation).
        let effective_env = isolation.effective_env(&self.id, self.generation, &spec.env);
        // Beta-audit P0.8: strict fails CLOSED — if the net namespace
        // cannot be proven, the launch is refused here (the error names
        // the clean downgrade for callers that want it), never silently
        // run networked.
        let (command, args, wrapper_available, network_isolated) =
            isolation.apply_to_command(&spec.command, &spec.args)?;
        let requested_network_ns =
            matches!(isolation, crate::session::isolation::Isolation::Strict);
        self.backend.set_clear_env(!matches!(
            isolation,
            crate::session::isolation::Isolation::Local
        ));
        // Re-attach any active recording hook (the backend was just replaced
        // internally on restart).
        self.backend.set_recording_hook(self.recording_slot.clone());
        // Wave F items 58–63: create the native semantic channel for this
        // generation and inject its path into the child's env. Apps that
        // never read TUI_LAB_SEMANTIC are unaffected; cooperative adapters
        // write their real tree there. A channel from a prior generation is
        // reset (same path, fresh content).
        if self.native.path.is_none() {
            self.native = crate::semantic::native::NativeChannel::create()
                .map_err(|e| anyhow::anyhow!("create native semantic channel: {e}"))?;
        } else {
            self.native.reset();
        }
        // Native event absorption restarts with the channel (re-review P1:
        // counts stay aligned across generations).
        self.event_state.set_native_absorbed_seq(0);
        let mut effective_env = effective_env; // native channel pair may append
        if let Some(pair) = self.native.env_pair() {
            if !crate::semantic::native::env_has_channel(&effective_env) {
                effective_env.push(pair);
            }
        }
        self.backend.start(
            &command,
            &args,
            spec.cwd.as_deref(),
            &effective_env,
            spec.cols,
            spec.rows,
        )?;
        self.caps_at_start = self.backend.capabilities();
        self.command = command.clone();
        // Evidence of what actually launched (item 77) — kept per generation.
        // `launch_succeeded` is set only here, after `backend.start()` above
        // returned Ok: the wrapper (if any) actually brought the child up.
        self.isolation_evidence = Some(crate::session::isolation::IsolationEvidence::for_profile(
            isolation,
            requested_network_ns,
            wrapper_available,
            network_isolated,
            true,
            &effective_env
                .iter()
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>(),
        ));
        self.launch = Some(spec);
        // A new process generation invalidates prior frame tracking.
        self.observation.clear();
        let s = self.backend.state()?;
        // Seed the window directly: the process-start event belongs to the
        // generation start (pushed below), not the first observe().
        let _ = self.observation.advance(s);
        // The process-start event belongs to the generation start, not the
        // first observation: `last` is seeded here, so an observe()-only
        // emission would never fire it.
        self.push_event(crate::events::TerminalEventKind::ProcessStarted);
        Ok(())
    }

    /// Restart the *same* logical session: reuse the stored [`LaunchSpec`]
    /// (spec section 13). Identity is preserved; generation is incremented so
    /// run artifacts / coverage / scenarios stay correlated.
    pub fn restart(&mut self) -> anyhow::Result<()> {
        let spec = match &self.launch {
            Some(s) => s.clone(),
            None => {
                return Err(anyhow::anyhow!(
                    "cannot restart session '{}': no prior launch spec",
                    self.id
                ))
            }
        };
        // Finding 39: restart consumes the SAME centralized stop lifecycle
        // as direct stop, so exit evidence and post-stop verification do
        // not depend on which caller killed the old generation. This
        // primitive already refuses to emit `ProcessExited` unless the
        // backend's post-stop state proves the owned child is gone (or the
        // attached detach is explicitly documented as leaving it alive).
        self.stop_lifecycle()?;
        // Reserve the NEW generation BEFORE the launch: scratch dirs, the
        // native channel reset, ProcessStarted, and isolation evidence are
        // all keyed on generation at start time. The old order launched N+1
        // under N's identity and bumped afterwards.
        let next = self.generation.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("generation counter exhausted for session '{}'", self.id)
        })?;
        // A failed launch is history: generation `next` was consumed by a
        // launch that did not come up. Do not silently retry under the old
        // number — evidence would collide.
        self.start_generation(spec, next)
    }

    /// THE lifecycle stop primitive (audit finding 39): stop, verify the
    /// post-stop process truth, emit the observed `ProcessExited` (or the
    /// documented attached-detach no-event case), then clear observation.
    /// Restart calls this BEFORE advancing generations; direct stop calls
    /// get the same lifecycle evidence regardless of caller.
    pub fn stop_lifecycle(&mut self) -> anyhow::Result<()> {
        self.backend.stop()?;
        let old_state = self
            .last_process_state
            .take()
            .unwrap_or_else(|| self.process());
        let post_state = self.process();
        let ownership = self.caps_at_start.process_ownership;
        let kill_on_stop = self.backend.kill_on_stop();
        if ownership == crate::backend::ProcessOwnership::Attached && !kill_on_stop {
            // Documented detach: a live pane outlives the session, and its
            // true exit is unobservable. Emit NO exit event.
            let _ = old_state;
        } else if post_state.running {
            // Never fabricate an exit for a stop that failed to terminate.
            return Err(anyhow::anyhow!(
                "cannot stop session '{}': stop() reported success but the child is still running",
                self.id
            ));
        } else {
            self.push_event(crate::events::TerminalEventKind::ProcessExited {
                exit_code: post_state.exit_code,
                exit_signal: post_state
                    .exit_signal
                    .or_else(|| old_state.running.then(|| "Terminated".to_string())),
            });
        }
        self.observation.clear();
        Ok(())
    }

    /// Direct stop. Uses the centralized lifecycle primitive so plain
    /// stop and restart share the same exit-evidence contract (finding 39).
    pub fn stop(&mut self) -> anyhow::Result<()> {
        self.stop_lifecycle()
    }

    /// Mutable backend access for capture-layer callers (frame-sequence
    /// capture walks the raw backend wait loop). Subsystem code should
    /// prefer the Session methods; this exists so `capture` can operate
    /// without duplicating session plumbing.
    pub fn backend_mut(&mut self) -> &mut dyn TerminalBackend {
        self.backend.as_mut()
    }

    /// Send input. When recording, the backend has already delivered the exact
    /// encoded bytes to the recording hook, so the cast shows the real bytes
    /// (audit item 25).
    pub fn send(&mut self, input: crate::backend::Input) -> anyhow::Result<()> {
        self.backend.send_input(input)?;
        Ok(())
    }

    /// Typed transport dispatch (audit findings 1/4): the ONE boundary the
    /// executor uses. The backend classifies whether the transport write was
    /// entered; the executor maps that onto [`DispatchStatus`] without
    /// re-inferring. `send()` remains for audit drivers that intentionally
    /// discard the classification.
    pub fn dispatch(
        &mut self,
        input: crate::backend::Input,
    ) -> crate::execution::DispatchResult<()> {
        let outcome = self.backend.dispatch(input);
        match outcome.result {
            Ok(()) => Ok(()),
            Err(e) => Err(crate::execution::DispatchError::from_backend(
                if outcome.write_entered {
                    crate::execution::DispatchStatus::PartialOrUnknown
                } else {
                    crate::execution::DispatchStatus::FailedBeforeWrite
                },
                e.to_string(),
            )),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // Backend owns resize recording (re-review item 3): the hook fires
        // only after the PTY resize actually succeeded, so a failed resize
        // is never recorded. The Session-level record here was a duplicate.
        self.backend.resize(cols, rows)?;
        if let Some(spec) = self.launch.as_mut() {
            spec.cols = cols;
            spec.rows = rows;
        }
        self.push_event(crate::events::TerminalEventKind::Resize { cols, rows });
        Ok(())
    }

    pub fn process(&mut self) -> ProcessState {
        self.backend.process()
    }

    /// Wave F item 53: true scrollback from the backend.
    pub fn backend_scrollback(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(self.backend.scrollback_lines()?)
    }

    /// Wave F item 53: search viewport + scrollback.
    pub fn backend_search(
        &mut self,
        query: &str,
    ) -> anyhow::Result<Vec<crate::backend::SearchHit>> {
        Ok(self.backend.search(query)?)
    }

    /// Wave F item 54: OSC 133 shell-integration state (None when the
    /// application emits no shell integration).
    pub fn backend_command_state(&mut self) -> Option<crate::backend::CommandState> {
        self.backend.command_state()
    }

    /// Current negotiated input modes (Wave 3 input-protocol audit): the
    /// same state `send` uses to choose key encodings, so an audit can
    /// report exactly what the engine will emit.
    pub fn input_modes(&self) -> crate::backend::InputModes {
        self.backend.input_modes()
    }

    /// Evidence-shaped startup readiness for the current generation.
    pub fn startup_outcome(&mut self) -> crate::backend::trait_def::StartupOutcome {
        self.backend.startup_outcome()
    }

    pub fn backend_version(&self) -> &'static str {
        match self.backend_kind {
            BackendKind::PortableVt => "portable-pty+vt100/0.1",
            BackendKind::PtyLine => "line-cli+lines/0.1",
            BackendKind::Pipe => "pipe/0.1",
            BackendKind::TmuxAttach => "tmux-attach/0.1",
        }
    }
}
