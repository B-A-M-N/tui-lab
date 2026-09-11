//! OS process + PTY infrastructure for the portable PTY backend
//! (god-object round 2, G2).
//!
//! The `PortablePtyBackend` fields that make up the child process and
//! its PTY plumbing — master, child, writer, reader thread handle, the
//! reader→pump chunk channel, the child pid, and the Wave G item 77
//! clear-env flag — move OUT of the backend into this holder. Process
//! policy only: spawn, teardown ordering, group signaling, process
//! state, chunk draining.
//!
//! No terminal semantics: the emulator grid, the event clock, the raw
//! ring, and the protocol callbacks all live elsewhere. The backend
//! calls `PtyProcess::spawn` from `start()`, `PtyProcess::drain`
//! from its single canonical `pump()`, and routes writes/resizes/
//! signals through the matching methods.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use portable_pty::{CommandBuilder, MasterPty, PtySize};

use crate::backend::{BackendError, BackendResult, RecordingHookSlot};
use crate::screen::ProcessState;

/// The child process + PTY plumbing for one backend instance.
pub(super) struct PtyProcess {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    writer: Option<Box<dyn Write + Send>>,
    /// Reader thread → main parse loop.
    reader_handle: Option<thread::JoinHandle<()>>,
    reader_rx: Option<mpsc::Receiver<Vec<u8>>>,
    child_pid: Option<u32>,
    /// Wave G item 77: clear the inherited env before applying pairs at
    /// the next spawn (clean/strict isolation profiles).
    clear_env_on_start: bool,
}

impl PtyProcess {
    pub(super) fn new() -> Self {
        PtyProcess {
            master: None,
            child: None,
            writer: None,
            reader_handle: None,
            reader_rx: None,
            child_pid: None,
            clear_env_on_start: false,
        }
    }

    /// Wave G item 77: clear the inherited environment before applying
    /// the caller's pairs on the next spawn (clean/strict isolation).
    pub(super) fn set_clear_env(&mut self, clear: bool) {
        self.clear_env_on_start = clear;
    }

    /// Whether a child session is attached (a writer exists to write to).
    pub(super) fn has_session(&self) -> bool {
        self.writer.is_some()
    }

    /// Open a PTY, spawn the child, and start the reader thread that
    /// forwards output chunks to the pump. The recording hook slot is
    /// cloned into the reader thread so output reaches the recorder
    /// (audit item 24) without involving the backend struct.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn(
        &mut self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        reader_hook: &RecordingHookSlot,
    ) -> BackendResult<()> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| BackendError::Spawn(e.to_string()))?;

        let mut cmd = CommandBuilder::new(command);
        for a in args {
            cmd.arg(a);
        }
        // portable-pty's CommandBuilder defaults a child with no explicit
        // cwd to $HOME — not the parent's cwd like std::process. A test
        // harness launching relative-path targets ("python3 fixtures/app.py")
        // from the project dir would silently run from ~. Inherit the
        // parent's cwd instead; an explicit `cwd` still wins.
        let cwd_owned: String;
        let cwd_arg: Option<&str> = match cwd {
            Some(c) => Some(c),
            None => match std::env::current_dir() {
                Ok(d) => {
                    cwd_owned = d.to_string_lossy().to_string();
                    Some(cwd_owned.as_str())
                }
                Err(_) => None,
            },
        };
        if let Some(c) = cwd_arg {
            cmd.cwd(c);
        }
        // Isolation profile (Wave G item 77): clean/strict launches drop the
        // inherited environment first so the child sees only the caller's
        // pairs. Local keeps portable-pty's base-env inheritance.
        if self.clear_env_on_start {
            cmd.env_clear();
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| BackendError::Spawn(e.to_string()))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| BackendError::Io(std::io::Error::other(e.to_string())))?;
        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
        // Clone the recording hook slot for the reader thread (audit 24).
        let reader_hook_for_thread = reader_hook.clone();
        let rh = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                            eprintln!("[tui-lab reader] EOF");
                        }
                        break;
                    }
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        // Feed the recording hook if attached.
                        if let Ok(slot) = reader_hook_for_thread.lock() {
                            if let Some(ref hook) = *slot {
                                hook.on_output(&chunk);
                            }
                        }
                        if chunk_tx.send(chunk).is_err() {
                            if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                                eprintln!("[tui-lab reader] channel closed, terminating");
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        if std::env::var("TUI_LAB_DEBUG_PTY").is_ok() {
                            eprintln!("[tui-lab reader] read error, terminating: {e}");
                        }
                        break;
                    }
                }
            }
        });

        // take writer
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| BackendError::Io(std::io::Error::other(e.to_string())))?;

        self.master = Some(pair.master);
        self.child = Some(child);
        self.writer = Some(writer);
        self.reader_handle = Some(rh);
        self.reader_rx = Some(chunk_rx);
        self.child_pid = self.child.as_ref().and_then(|c| c.process_id());
        Ok(())
    }

    /// Drain the output chunks currently buffered by the reader thread
    /// (non-blocking). The pump is the only caller.
    pub(super) fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut drained = Vec::new();
        if let Some(rx) = self.reader_rx.as_ref() {
            while let Ok(chunk) = rx.try_recv() {
                drained.push(chunk);
            }
        }
        drained
    }

    /// Write bytes to the PTY.
    pub(super) fn write(&mut self, bytes: &[u8]) -> BackendResult<()> {
        let writer = match self.writer.as_mut() {
            Some(w) => w,
            None => return Err(BackendError::NoSession),
        };
        writer.write_all(bytes).map_err(BackendError::Io)?;
        writer.flush().map_err(BackendError::Io)?;
        Ok(())
    }

    /// Resize the OS PTY (the caller resizes the emulator grid to match).
    pub(super) fn resize_master(&mut self, cols: u16, rows: u16) -> BackendResult<()> {
        if let Some(master) = self.master.as_ref() {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| BackendError::Spawn(e.to_string()))?;
        }
        Ok(())
    }

    /// Deliver a POSIX signal to the child's process group so spawned
    /// subprocesses are also signalled (spec section 7), falling back to
    /// the direct child when group signaling fails.
    #[cfg(unix)]
    pub(super) fn signal_group(&self, sig: i32) -> BackendResult<()> {
        match self.child_pid {
            Some(pid) => {
                // Negative pid targets the process group (killpg semantics).
                let r = unsafe { libc::kill(-(pid as i32), sig) };
                if r != 0 {
                    // fall back to killing the direct child only
                    let _ = unsafe { libc::kill(pid as i32, sig) };
                }
                Ok(())
            }
            None => Err(BackendError::NoSession),
        }
    }

    #[cfg(not(unix))]
    pub(super) fn signal_group(&self, _sig: i32) -> BackendResult<()> {
        Err(BackendError::Unsupported(
            "arbitrary POSIX signals are only available on Unix".into(),
        ))
    }

    /// Snapshot the child's process state (spec-facing `ProcessState`).
    pub(super) fn process_state(&mut self) -> ProcessState {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.exit_code() as i32;
                    let sig = status.signal().map(|s| s.to_string());
                    ProcessState {
                        running: false,
                        exit_code: Some(code),
                        exit_signal: sig,
                        cwd: None,
                        pid: self.child_pid,
                    }
                }
                _ => ProcessState {
                    running: true,
                    exit_code: None,
                    exit_signal: None,
                    cwd: None,
                    pid: self.child_pid,
                },
            },
            None => ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    /// Tear the session down: signal the child's process group, kill and
    /// reap the direct child, then drop writer/master so the reader hits
    /// EOF, and join it. Ordering matters — the group signal precedes the
    /// kill so grandchildren get a chance to react, and the reader join
    /// must come after the master is dropped or it never sees EOF.
    pub(super) fn terminate(&mut self) {
        if let Some(pid) = self.child_pid {
            #[cfg(unix)]
            unsafe {
                // Negative pid targets the process group (killpg semantics).
                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
            }
            #[cfg(not(unix))]
            let _ = pid;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Drop writer/master so the reader hits EOF.
        self.writer = None;
        self.master = None;
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
        self.reader_rx = None;
        self.child_pid = None;
    }
}
