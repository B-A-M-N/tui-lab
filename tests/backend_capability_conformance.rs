//! Per-backend CAPABILITY conformance suite (audit finding 61).
//!
//! For EVERY backend (portable PTY, line CLI, pipe, tmux attach) this drives
//! the capability matrix the backend advertises and holds it honest:
//!
//!   * every capability reported `true` → exercise a representative operation
//!     and assert it actually works;
//!   * every known-unsupported operation → make the request and assert it
//!     fails IMMEDIATELY as `BackendError::Unsupported`, not after hanging its
//!     waited budget.
//!
//! It also pins the matrix-consistency invariant (audit finding 57): a
//! backend that advertises `protocol_capture` must list `Raw` event/input
//! coverage, a `title` backend must list `Title` waits/events, etc. This is
//! the suite that would have caught the historical tmux title/bell/raw/
//! search inconsistencies — so tmux is driven by its genuine capability
//! matrix rather than assumption.

use std::time::{Duration, Instant};

use tui_lab::backend::line_cli::PtyLineBackend;
use tui_lab::backend::pipe::PipeBackend;
use tui_lab::backend::portable_pty::PortablePtyBackend;
use tui_lab::backend::tmux::TmuxBackend;
use tui_lab::backend::{
    BackendError, BackendResult, Capabilities, EventCapability, Input, InputFamily, KeyCode,
    KeyEvent, KeyModifiers, MouseButton, TerminalBackend, WaitCapability, WaitCond,
};

/// A child that prints several distinct lines then idles — enough history for
/// a scrollback exercise and confirmation the screen received output.
const LINES_CHILD: &str = "\
import sys, time
for i in range(1, 6):
    print(f'ROW{i}', flush=True)
time.sleep(2)
";

/// A child that writes to BOTH stdout and stderr, so the pipe backend's
/// separation is observable.
const SPLIT_CHILD: &str = "\
import sys, time
sys.stdout.write('ONOUT'); sys.stdout.flush()
sys.stderr.write('ONERR'); sys.stderr.flush()
time.sleep(2)
";

/// A child that exits immediately with a real, non-zero exit code.
const EXIT_CHILD: &str = "\
import sys
sys.stdout.write('exiting now\\n'); sys.stdout.flush()
sys.exit(3)
";

/// A child that rings the terminal bell.
const BELL_CHILD: &str = "\
import sys, time
sys.stdout.write('\\a'); sys.stdout.flush()
time.sleep(2)
";

/// Spawn `b` against a `python3` child running `script`.
fn spawn(b: &mut dyn TerminalBackend, script: &str) {
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start python child");
}

/// Run `op` and assert it returns `Err(BackendError::Unsupported)` AND fast —
/// a backend that can't do the operation must fail immediately, never sit
/// through a wait budget (finding 37 preflight + finding 61).
fn check_fast_unsupported(
    b: &mut dyn TerminalBackend,
    label: &str,
    op: fn(&mut dyn TerminalBackend) -> BackendResult<()>,
) {
    let start = Instant::now();
    let r = op(b);
    let elapsed = start.elapsed();
    match r {
        Err(BackendError::Unsupported(_)) => {}
        other => panic!("{label}: expected fast Unsupported, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_millis(1500),
        "{label}: unsupported request took {elapsed:?} — it must fail immediately, not hang through a timeout"
    );
}

/// The representative operations that back the advertized boolean
/// capabilities. Exercised only when the flag is `true`. (exit-code and
/// stdout/stderr separation, which need their own child or a downcast, are
/// handled per-backend.)
fn exercise_positive(b: &mut dyn TerminalBackend, caps: &Capabilities) {
    if caps.raw_input {
        b.send_input(Input::Raw(vec![b'x']))
            .expect("raw_input=true must accept a raw byte write");
    }
    if caps.protocol_capture {
        b.recent_raw_output()
            .expect("protocol_capture=true must read the raw ring");
    }
    if caps.scrollback {
        let rows = b
            .scrollback_lines()
            .expect("scrollback=true must return rows");
        assert!(
            !rows.is_empty(),
            "scrollback=true must report retained lines, got none"
        );
    }
    if caps.signals && cfg!(unix) {
        // Signal 0 is a no-op existence check — acceptance proves the
        // signal path is wired without harming the child.
        b.send_input(Input::Signal(0))
            .expect("signals=true must accept a signal delivery");
    }
}

/// Capability-matrix internal consistency (audit finding 57): advertised
/// booleans must line up with the list coverage that would back them.
fn check_matrix_consistency(caps: &Capabilities) {
    if caps.protocol_capture {
        assert!(
            caps.event_types.contains(&EventCapability::Raw),
            "protocol_capture=true must list Raw events"
        );
        assert!(
            caps.input_families.contains(&InputFamily::RawByte),
            "protocol_capture=true must list raw input"
        );
    }
    if caps.title {
        assert!(
            caps.supported_waits.contains(&WaitCapability::Title),
            "title=true must support Title waits"
        );
        assert!(
            caps.event_types.contains(&EventCapability::Title),
            "title=true must observe Title events"
        );
    }
    if caps.bell_observable {
        assert!(
            caps.supported_waits.contains(&WaitCapability::Bell),
            "bell_observable=true must support Bell waits"
        );
        assert!(
            caps.event_types.contains(&EventCapability::Bell),
            "bell_observable=true must observe Bell events"
        );
    }
    if caps.shell_integration {
        assert!(
            caps.supported_waits.contains(&WaitCapability::CommandDone),
            "shell_integration=true must support command waits"
        );
    }
    if caps.signals {
        assert!(
            caps.input_families.contains(&InputFamily::Signal),
            "signals=true must list signal input"
        );
    }
    if caps.mouse {
        assert!(
            caps.input_families.contains(&InputFamily::Mouse),
            "mouse=true must list mouse input"
        );
    }
}

// ── portable PTY ─────────────────────────────────────────────────────────

#[test]
fn portable_capabilities_are_backed_by_real_operations() {
    let mut b = PortablePtyBackend::new(80, 24);
    spawn(&mut b, LINES_CHILD);
    let caps = b.capabilities();
    // The reference backend advertises every operation it can genuinely do.
    assert!(
        caps.raw_input && caps.protocol_capture,
        "portable does raw I/O"
    );
    assert!(
        caps.bell_observable && caps.exit_code,
        "portable observes bell + exit"
    );
    assert!(caps.shell_integration, "portable parses OSC 133");
    assert!(caps.recording, "portable records casts");
    assert!(caps.query_response, "portable answers device queries");
    assert!(!caps.attach, "portable spawns, it does not attach");
    assert!(caps.event_types.contains(&EventCapability::Raw));
    assert!(caps.supported_waits.contains(&WaitCapability::CommandDone));
    assert!(caps.input_families.contains(&InputFamily::Mouse));
    exercise_positive(&mut b, &caps);
    check_matrix_consistency(&caps);
    b.stop().expect("stop");

    // bell_observable=true → a BEL is observed via wait(Bell).
    let mut bell = PortablePtyBackend::new(80, 24);
    spawn(&mut bell, BELL_CHILD);
    let out = bell
        .wait(
            WaitCond::Bell {
                after_bell_seq: None,
            },
            Duration::from_secs(5),
        )
        .expect("bell wait");
    assert!(out.met, "bell_observable=true must observe a real BEL");
    bell.stop().expect("stop");

    // exit_code=true → a child exiting reports a real code.
    let mut exit_b = PortablePtyBackend::new(80, 24);
    spawn(&mut exit_b, EXIT_CHILD);
    let out = exit_b
        .wait(WaitCond::ProcessExit, Duration::from_secs(5))
        .expect("exit wait");
    assert!(out.met, "exit_code=true backend must report child exit");
    let st = exit_b.state().expect("state");
    assert_eq!(
        st.process.exit_code,
        Some(3),
        "real exit code must be reported"
    );
    exit_b.stop().expect("stop");
}

// ── line CLI ─────────────────────────────────────────────────────────────

#[test]
fn line_cli_capabilities_are_backed_by_real_operations() {
    let mut b = PtyLineBackend::new(80, 24);
    spawn(&mut b, LINES_CHILD);
    let caps = b.capabilities();
    assert!(
        caps.protocol_capture && caps.scrollback,
        "line keeps history + raw ring"
    );
    exercise_positive(&mut b, &caps);
    check_matrix_consistency(&caps);

    // Known-unsupported ops fail fast (finding 61): mouse + modified keys.
    check_fast_unsupported(&mut b, "line CLI mouse", |b| {
        b.send_input(Input::MouseClick {
            button: MouseButton::Left,
            x: 1,
            y: 1,
        })
    });
    check_fast_unsupported(&mut b, "line CLI modified key", |b| {
        b.send_input(Input::Key(KeyEvent::with_modifiers(
            KeyCode::Char('c'),
            KeyModifiers::CTRL,
        )))
    });

    // A condition the line engine does NOT list in supported_waits (title)
    // fails the finding-37 preflight helper fast rather than timing out.
    assert!(!caps.supported_waits.contains(&WaitCapability::Title));
    assert!(caps.require_wait(&WaitCond::Title("x".into())).is_err());
    b.stop().expect("stop");
}

// ── pipe ─────────────────────────────────────────────────────────────────

#[test]
fn pipe_capabilities_are_backed_by_real_operations() {
    let mut b = PipeBackend::new(80, 24);
    spawn(&mut b, LINES_CHILD);
    let caps = b.capabilities();
    assert!(caps.stdout_stderr_separation, "pipe splits stdout/stderr");
    exercise_positive(&mut b, &caps);
    check_matrix_consistency(&caps);
    b.stop().expect("stop");

    // stdout/stderr separation actually separates the two streams.
    let mut pb = PipeBackend::new(80, 24);
    spawn(&mut pb, SPLIT_CHILD);
    std::thread::sleep(Duration::from_millis(300));
    let pipe = pb
        .as_any_mut()
        .downcast_mut::<PipeBackend>()
        .expect("pipe downcast");
    let out = pipe.stdout_lines().concat();
    let err = pipe.stderr_lines().concat();
    assert!(
        out.contains("ONOUT"),
        "stdout must carry stdout: {out:?} {err:?}"
    );
    assert!(
        err.contains("ONERR"),
        "stderr must carry stderr: {out:?} {err:?}"
    );
    pb.stop().expect("stop");

    // exit_code=true → the pipe child's real exit code is reported.
    let mut exit_b = PipeBackend::new(80, 24);
    spawn(&mut exit_b, EXIT_CHILD);
    let out = exit_b
        .wait(WaitCond::ProcessExit, Duration::from_secs(5))
        .expect("pipe exit wait");
    assert!(
        out.met,
        "pipe exit_code=true backend must report child exit"
    );
    assert_eq!(
        out.state.process.exit_code,
        Some(3),
        "pipe must report the real exit code"
    );

    // Negative: mouse + modified keys fail fast on the pipe engine too.
    check_fast_unsupported(&mut exit_b, "pipe mouse", |b| {
        b.send_input(Input::MouseClick {
            button: MouseButton::Left,
            x: 1,
            y: 1,
        })
    });
    check_fast_unsupported(&mut exit_b, "pipe modified key", |b| {
        b.send_input(Input::Key(KeyEvent::with_modifiers(
            KeyCode::Char('x'),
            KeyModifiers::SHIFT,
        )))
    });
    exit_b.stop().expect("stop");
}

// ── tmux attach ──────────────────────────────────────────────────────────

/// Start a throwaway tmux session; `None` when tmux is unavailable (then the
/// tmux test SKIPS, matching how the project treats optional integrations).
fn start_tmux() -> Option<String> {
    let name = format!("tuilab-caps-{}", std::process::id());
    let out = std::process::Command::new("tmux")
        .args(["new-session", "-d", "-s", &name])
        .output()
        .ok()?;
    if out.status.success() {
        Some(name)
    } else {
        None
    }
}

#[test]
fn tmux_capabilities_are_honest_and_backed() {
    let Some(sess) = start_tmux() else {
        eprintln!("SKIP: tmux unavailable — skipping tmux capability conformance");
        return;
    };
    let target = format!("{sess}:0.0");
    let mut b = match TmuxBackend::attach(&target, 80, 24) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &sess])
                .output();
            panic!("attach to {target} failed: {e}");
        }
    };

    let caps = b.capabilities();
    // The historical inconsistencies this suite must catch (audit findings
    // 38/39/40): title is genuinely observable, but raw bytes, signals, mouse,
    // exit codes and protocol capture are genuinely absent.
    assert!(
        caps.title && caps.attach,
        "tmux observes pane titles and attaches"
    );
    assert!(
        !caps.raw_input,
        "tmux send-keys cannot deliver arbitrary raw bytes"
    );
    assert!(
        !caps.exit_code,
        "tmux cannot report the (non-child) exit code"
    );
    assert!(!caps.signals, "tmux cannot signal the attached process");
    assert!(!caps.mouse, "tmux pane mouse injection is unsupported");
    assert!(
        !caps.protocol_capture,
        "tmux mediates the byte stream (no raw capture)"
    );
    assert!(!caps.shell_integration, "tmux has no OSC 133 command state");
    assert!(
        !caps.supported_waits.contains(&WaitCapability::CommandDone),
        "tmux has no command waits"
    );
    assert!(caps.event_types.contains(&EventCapability::Title));
    check_matrix_consistency(&caps);

    // Positive: a real pane is attached and capturable (scrollback present).
    assert!(
        caps.scrollback,
        "tmux exposes its own history buffer as scrollback"
    );
    b.state().expect("tmux state reads the pane");
    b.scrollback_lines().expect("tmux scrollback_lines");

    // Negative (find finding 61 fast-fail):
    check_fast_unsupported(&mut b, "tmux non-UTF-8 raw", |b| {
        b.send_input(Input::Raw(vec![0xff, 0x00, 0xff]))
    });
    check_fast_unsupported(&mut b, "tmux signal", |b| b.send_input(Input::Signal(15)));
    check_fast_unsupported(&mut b, "tmux mouse", |b| {
        b.send_input(Input::MouseClick {
            button: MouseButton::Left,
            x: 1,
            y: 1,
        })
    });
    check_fast_unsupported(&mut b, "tmux command wait", |b| {
        b.wait(
            WaitCond::CommandDone {
                after_command_seq: None,
            },
            Duration::from_secs(30),
        )
        .map(|_| ())
    });

    b.stop().expect("tmux detach");
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sess])
        .output();
}
