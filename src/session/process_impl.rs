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
        spec: LaunchSpec,
    ) {
        self.backend = backend;
        self.backend_kind = kind;
        self.launch = Some(spec);
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
        // Record the old generation's death BEFORE the backend drops the
        // child: without this, a process killed by a restart vanishes from
        // the event ledger with no death record — indistinguishable from
        // "still running" and a provenance gap exactly like the un-recorded
        // failed-launch case this function already guards against. If the
        // process was already dead, `process()` carries its real code or
        // signal; if it was alive, the stop below terminates it (SIGTERM is
        // stop()'s mechanism), and the record says so.
        //
        // Finding 6 (ownership truth): the "we terminated it" claim is only
        // legal for a SPAWNED child. An ATTACHED target (tmux pane) is not
        // ours to signal — unless the operator explicitly asked for
        // kill-on-stop, the detach leaves it running, and the ledger must
        // not say "Terminated" about a process that is still alive.
        let old_state = self.backend.process();
        let ownership = self.backend.capabilities().process_ownership;
        let kill_on_stop = self.backend.kill_on_stop();
        self.backend.stop().ok();
        if ownership == crate::backend::ProcessOwnership::Attached && !kill_on_stop {
            if old_state.running {
                // Detached from a live pane: NO exit event — the process
                // outlives the session and its true exit is unobservable
                // from here.
            } else {
                self.push_event(crate::events::TerminalEventKind::ProcessExited {
                    exit_code: None,
                    exit_signal: old_state.exit_signal,
                });
            }
        } else if old_state.running {
            self.push_event(crate::events::TerminalEventKind::ProcessExited {
                exit_code: None,
                exit_signal: Some("Terminated".to_string()),
            });
        } else {
            self.push_event(crate::events::TerminalEventKind::ProcessExited {
                exit_code: old_state.exit_code,
                exit_signal: old_state.exit_signal,
            });
        }
        // Reserve the NEW generation BEFORE the launch: scratch dirs, the
        // native channel reset, ProcessStarted, and isolation evidence are
        // all keyed on generation at start time. The old order launched N+1
        // under N's identity and bumped afterwards.
        let next = self.generation.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("generation counter exhausted for session '{}'", self.id)
        })?;
        match self.start_generation(spec, next) {
            Ok(()) => Ok(()),
            Err(e) => {
                // The failed launch is recorded as history: generation `next`
                // was consumed by a launch that did not come up. Do NOT
                // silently retry under the old number (evidence would collide).
                Err(e)
            }
        }
    }

    pub fn stop(&mut self) -> anyhow::Result<()> {
        self.backend.stop()?;
        self.observation.clear();
        Ok(())
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
