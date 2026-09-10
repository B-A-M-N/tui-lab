//! "Prove the whole thing, not the pieces" — item 40.
//!
//! Byte-level backend matrix: for every engine the harness ships, the
//! exact bytes that cross the child's stdin must match what the requested
//! key/mouse encoding demands, and the bytes decoded from the child's
//! stdout must arrive honestly. Item 23 pinned the portable-pty engine;
//! this file extends the same hex-echo methodology across pipe, line-cli,
//! and tmux-attach — and pins the honest `Unsupported` rejections where an
//! engine genuinely cannot express an input.

use std::time::Duration;

use tui_lab::backend::{
    BackendError, Input, KeyCode, KeyEvent, MouseButton, PipeBackend, PtyLineBackend,
    TerminalBackend, WaitCond,
};

/// A python child that reads N raw stdin bytes and prints them as hex.
/// Works identically under the pipe engine (plain fds) and the line engine
/// (real PTY; setraw only where a tty exists, and read() is byte-exact
/// either way).
const HEX_ECHO: &str = "\
import sys, tty, time
try:
    tty.setraw(0)
except Exception:
    pass  # pipe engine: no tty, plain fds are already byte-exact
data = sys.stdin.buffer.read(N)
print('HEX:' + data.hex(), flush=True)
import time as _t; _t.sleep(30)
";

fn hex_echo(n: usize) -> String {
    HEX_ECHO.replace("N", &n.to_string())
}

/// Wait for the HEX readback with a bounded, descriptive budget.
fn wait_hex<T: TerminalBackend>(b: &mut T, want: &str) {
    let out = b
        .wait(
            WaitCond::Text(format!("HEX:{want}")),
            Duration::from_secs(10),
        )
        .expect("wait for hex readback");
    assert!(
        out.met,
        "expected bytes {want} never appeared; screen={:?}",
        out.state.viewport_text.join("|")
    );
}

// ── pipe engine ──────────────────────────────────────────────────────────

fn pipe(script: &str) -> PipeBackend {
    let mut b = PipeBackend::new(80, 24);
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start pipe child");
    b
}

/// Raw bytes cross the pipe byte-exactly — the engine's whole point.
#[test]
fn pipe_bytes_text_is_byte_exact() {
    let mut b = pipe(&hex_echo(5));
    // Let the child reach its read().
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Text("aB\x7f\x1bZ".into()))
        .expect("send text");
    // 'a'=61 'B'=42 DEL=7f ESC=1b 'Z'=5a — the pipe delivers every byte
    // verbatim, including the C0 and escape bytes a terminal would echo.
    wait_hex(&mut b, "61427f1b5a");
    b.stop().expect("stop");
}

/// Char keys degrade to their character byte (a pipe has no key semantics
/// beyond bytes) and Enter is one 0x0a byte — the pipe never invents the
/// CR a PTY would.
#[test]
fn pipe_bytes_char_and_enter() {
    let mut b = pipe(&hex_echo(4));
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Keys(vec![
        KeyEvent::new(KeyCode::Char('x')),
        KeyEvent::new(KeyCode::Enter),
        KeyEvent::new(KeyCode::Char('y')),
        KeyEvent::new(KeyCode::Char('!')),
    ]))
    .expect("send keys");
    wait_hex(&mut b, "780a7921");
    b.stop().expect("stop");
}

/// Modifiers and navigation keys are NOT silently degraded to a wrong
/// byte: ctrl+c on a pipe is rejected as Unsupported, not sent as 'c'.
#[test]
fn pipe_rejects_unencodable_keys_loudly() {
    let mut b = pipe(&hex_echo(1));
    std::thread::sleep(Duration::from_millis(500));
    let err = b
        .send_input(Input::Key(KeyEvent::with_modifiers(
            KeyCode::Char('c'),
            tui_lab::backend::KeyModifiers::CTRL,
        )))
        .expect_err("ctrl+c must be rejected on a pipe");
    assert!(
        matches!(err, BackendError::Unsupported(_)),
        "rejection must be explicit: {err:?}"
    );
    // The child never receives a byte from the rejected send: the pending
    // read gets exactly the one byte we now send legitimately.
    b.send_input(Input::Text("q".into())).expect("send");
    wait_hex(&mut b, "71");
    b.stop().expect("stop");
}

/// Mouse is meaningless without a terminal grid — rejected, never faked.
#[test]
fn pipe_rejects_mouse() {
    let mut b = pipe(&hex_echo(1));
    std::thread::sleep(Duration::from_millis(500));
    let err = b
        .send_input(Input::MouseClick {
            button: MouseButton::Left,
            x: 3,
            y: 4,
        })
        .expect_err("mouse must be rejected on a pipe");
    assert!(matches!(err, BackendError::Unsupported(_)), "{err:?}");
    b.stop().expect("stop");
}

// ── line-cli engine ──────────────────────────────────────────────────────

fn line(script: &str) -> PtyLineBackend {
    let mut b = PtyLineBackend::new(80, 24);
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start line child");
    b
}

/// Line-CLI is a real PTY behind a line model: chars cross as their bytes
/// and Enter arrives as the newline byte the engine encodes (0x0a — the
/// deliberate line-backend encoding, distinct from portable-pty's CR).
#[test]
fn line_cli_bytes_char_and_enter() {
    let mut b = line(&hex_echo(3));
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Keys(vec![
        KeyEvent::new(KeyCode::Char('h')),
        KeyEvent::new(KeyCode::Char('i')),
        KeyEvent::new(KeyCode::Enter),
    ]))
    .expect("send keys");
    wait_hex(&mut b, "68690a");
    b.stop().expect("stop");
}

/// Input::Text crosses byte-exactly (the CLI path's core job: drive REPLs
/// and prompts with literal text).
#[test]
fn line_cli_bytes_text() {
    let mut b = line(&hex_echo(6));
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Text("say hi\n".into()))
        .expect("send text");
    wait_hex(&mut b, "736179206869");
    // Text already carried the newline; read(6) consumed exactly the six
    // bytes before it.
    b.stop().expect("stop");
}

/// Full key semantics do not exist on this engine: an arrow key must be
/// REJECTED, never silently degraded to some escape sequence the app would
/// misread as literal input.
#[test]
fn line_cli_rejects_arrow_keys_loudly() {
    let mut b = line(&hex_echo(1));
    std::thread::sleep(Duration::from_millis(500));
    let err = b
        .send_input(Input::Key(KeyEvent::new(KeyCode::Up)))
        .expect_err("arrow keys must be rejected on the line engine");
    assert!(
        matches!(err, BackendError::Unsupported(_)),
        "rejection must be explicit: {err:?}"
    );
    b.stop().expect("stop");
}

// ── tmux-attach engine ───────────────────────────────────────────────────

/// A real tmux pane whose inner app reports the exact bytes it received.
/// tmux mediates the stream, so the assertion is on what the app READ —
/// the ground truth "what did injection actually deliver".
#[test]
fn tmux_bytes_named_keys_and_literal_text() {
    let probe = std::process::Command::new("tmux")
        .args(["list-sessions"])
        .output();
    if probe.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        let reason = probe
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_else(|e| e.to_string());
        if reason.contains("Operation not permitted") || reason.contains("No such file") {
            eprintln!("SKIP: tmux server/socket unavailable in this sandbox: {reason}");
            return;
        }
    }
    let sess_name = format!("tuilab-bytes-{}", std::process::id());
    // The inner app: reads 3 raw bytes, prints their hex, keeps reading.
    let out = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &sess_name,
            "-x",
            "80",
            "-y",
            "24",
            "python3",
            "-u",
            "-c",
            "\
import sys, tty
tty.setraw(0)
while True:
    d = sys.stdin.buffer.read(3)
    if not d:
        break
    print('HEX:' + d.hex(), flush=True)
",
        ])
        .output()
        .expect("spawn tmux session");
    assert!(
        out.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let pool = tui_lab::session::SessionPool::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let id = pool
            .attach_tmux(&format!("{sess_name}:0.0"), 80, 24)
            .await
            .expect("attach");
        pool.with_session(Some(&id), |s| {
            // Give the inner app a moment to reach setraw+read.
            std::thread::sleep(Duration::from_millis(600));
            // Literal text goes through `send-keys -l` (byte-exact); named
            // keys are real keypresses. Sending two literal chars plus a
            // named Enter must deliver 0x61 0x62 0x0d — tmux turns a named
            // Enter into the CR byte a real Enter produces.
            s.send(Input::Keys(vec![
                KeyEvent::new(KeyCode::Char('a')),
                KeyEvent::new(KeyCode::Char('b')),
                KeyEvent::new(KeyCode::Enter),
            ]))
            .expect("send keys");
            let out = s
                .wait(WaitCond::Text("HEX:61620d".into()), 10_000)
                .expect("wait for hex");
            assert!(
                out.met,
                "literal a,b + named Enter must deliver 61620d; screen={:?}",
                out.state.viewport_text.join("|")
            );
            // Arrow keys are named presses in tmux: Right is a REAL Right
            // keypress, which arrives as the CSI C sequence tmux synthesizes.
            s.send(Input::Key(KeyEvent::new(KeyCode::Right)))
                .expect("send right");
            let out = s
                .wait(WaitCond::Text("HEX:1b5b43".into()), 10_000)
                .expect("wait for CSI C");
            assert!(
                out.met,
                "named Right must arrive as ESC [ C through tmux: {:?}",
                out.state.viewport_text.join("|")
            );
        })
        .await
        .expect("actor");
    });

    // The user's pane must outlive detach (item 18's contract, re-asserted
    // here so byte-level teardown keeps it).
    rt.block_on(async {
        pool.with_session(Some(pool.active_id().as_deref().unwrap_or("")), |s| {
            s.stop().expect("stop");
        })
        .await
        .expect("detach");
    });
    let alive = std::process::Command::new("tmux")
        .args(["has-session", "-t", &sess_name])
        .output()
        .expect("has-session");
    assert!(alive.status.success(), "detach must not kill the pane");
    std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sess_name])
        .output()
        .ok();
}

/// stdout decoding across every engine feeds the same screen model: the
/// pipe engine's stdout/stderr separation is byte-honest, per stream.
#[test]
fn pipe_stdout_stderr_stay_separable() {
    let mut b = pipe(
        "import sys\n\
         sys.stdout.write('OUT-1\\n')\n\
         sys.stderr.write('ERR-1\\n')\n\
         sys.stdout.flush(); sys.stderr.flush()\n\
         import time; time.sleep(30)",
    );
    let out = b
        .wait(WaitCond::Text("ERR-1".into()), Duration::from_secs(10))
        .expect("wait for stderr line on the fused screen");
    assert!(out.met, "fused screen interleaves both streams");
    // The separable stores keep the provenance honest.
    let stdout_lines = b.stdout_lines();
    let stderr_lines = b.stderr_lines();
    assert!(
        stdout_lines.iter().any(|l| l.contains("OUT-1")),
        "stdout store: {stdout_lines:?}"
    );
    assert!(
        !stdout_lines.iter().any(|l| l.contains("ERR-1")),
        "stderr must never leak into the stdout store: {stdout_lines:?}"
    );
    assert!(
        stderr_lines.iter().any(|l| l.contains("ERR-1")),
        "stderr store: {stderr_lines:?}"
    );
    b.stop().expect("stop");
}
