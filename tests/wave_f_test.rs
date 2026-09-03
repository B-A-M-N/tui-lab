//! Wave F integration suite (items 50–64): borrowed-system integrations
//! proven against real python children through the real backends.
//!
//! Every test exercises the trait boundary or the MCP session path — no
//! mocks — because these features are only real when a live process
//! negotiates them.

use std::time::Duration;

use tui_lab::backend::line_cli::PtyLineBackend;
use tui_lab::backend::pipe::PipeBackend;
use tui_lab::backend::portable_pty::PortablePtyBackend;
use tui_lab::backend::{
    Capabilities, CommandState, Input, KeyCode, KeyEvent, SearchHit, TerminalBackend, WaitCond,
};

/// Spawn a child that prints lines slowly (CLI-style output).
fn cli_backend(script: &str) -> PtyLineBackend {
    let mut b = PtyLineBackend::new(80, 24);
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start python");
    b
}

/// Spawn a child under genuine pipes (no PTY) — `isatty()` is false for the
/// child, which is the whole point of the pipe transport.
fn pipe_backend(script: &str) -> PipeBackend {
    let mut b = PipeBackend::new(80, 24);
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start python via pipes");
    b
}

fn pty_backend(script: &str) -> PortablePtyBackend {
    let mut b = PortablePtyBackend::new(80, 24);
    b.start(
        "python3",
        &["-c".to_string(), script.to_string()],
        None,
        &[],
        80,
        24,
    )
    .expect("start python");
    b
}

// ── Items 50–51: PtyLineBackend ─────────────────────────────────────────

/// W1b: resize on the line backend is a REAL winsize push (the backend is a
/// genuine PTY). The child must observe the new COLUMNS via TIOCGWINSZ —
/// the old synthetic-only resize never delivered WINCH, so a resize-aware
/// child kept rendering at the old width forever.
#[test]
fn line_cli_resize_delivers_winsize_to_child() {
    let mut b = cli_backend(
        "import sys, struct, termios, fcntl, time\n\
         def cols():\n\
         \x20   return struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\\x00'*8))[1]\n\
         sys.stdout.write(f'C0-{cols()}\\n'); sys.stdout.flush()\n\
         time.sleep(2.0)\n\
         sys.stdout.write(f'C1-{cols()}\\n'); sys.stdout.flush()\n\
         time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("C0-".into()), Duration::from_secs(5));
    b.resize(110, 30).expect("resize");
    let out = b
        .wait(WaitCond::Text("C1-110".into()), Duration::from_secs(5))
        .expect("wait post-resize cols");
    assert!(out.met, "child must see 110 cols after real resize");
    b.stop().ok();
}

#[test]
fn line_cli_output_becomes_lines_with_scrollback() {
    let mut b = cli_backend(
        "import sys,time\n\
         for i in range(30):\n\
         \x20   print(f'line-{i}')\n\
         \x20   sys.stdout.flush()\n\
         \x20   time.sleep(0.02)\n\
         time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("line-29".into()), Duration::from_secs(10));
    let st = b.state().expect("state");
    // 24-row viewport: line-6..line-29 visible; earlier lines scrollback.
    assert!(
        st.viewport_text.iter().any(|r| r.contains("line-29")),
        "last line in viewport: {:?}",
        st.viewport_text.last()
    );
    assert!(
        st.scrollback.iter().any(|r| r.contains("line-0")),
        "first line in scrollback: {} scrollback rows",
        st.scrollback.len()
    );
    // Honest capability: scrollback=true on the CLI backend.
    let caps = b.capabilities();
    assert!(caps.scrollback, "cli backend retains history");
    assert!(!caps.mouse, "no mouse on pipes");
    assert!(!caps.colors, "no style claims for plain line output");
    b.stop().ok();
}

#[test]
fn line_cli_text_input_reaches_child() {
    let mut b = cli_backend(
        "import sys\n\
         sys.stdout.write('NAME: ') ; sys.stdout.flush()\n\
         name = sys.stdin.readline().strip()\n\
         print(f'HELLO {name}')\n\
         sys.stdout.flush()\n\
         import time; time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("NAME:".into()), Duration::from_secs(5));
    b.send_input(Input::Text("ada\n".into())).expect("send");
    let out = b
        .wait(WaitCond::Text("HELLO ada".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(out.met, "child must see the piped input");
    b.stop().ok();
}

/// Regression for review item "unterminated prompts lose visibility":
/// a prompt that has not received a newline must be rendered as the live
/// viewport line, and its mutation (no `\n`) must advance `screen_seq` so an
/// anchored ScreenStable/ScreenChange wait is satisfied by a real visible
/// change rather than sleeping past it.
#[test]
fn line_cli_pending_prompt_is_visible_and_moves_screen_seq() {
    let mut b = cli_backend(
        "import sys,time\n\
         sys.stdout.write('Enter value> ') ; sys.stdout.flush()\n\
         time.sleep(0.8)\n\
         sys.stdout.write('Enter value> x') ; sys.stdout.flush()\n\
         time.sleep(0.8)\n\
         print('done')\n\
         time.sleep(1)",
    );
    // First wait: the unterminated prompt appears even with no newline yet.
    let out = b
        .wait(
            WaitCond::Text("Enter value> ".into()),
            Duration::from_secs(5),
        )
        .expect("wait prompt");
    assert!(out.met, "unterminated prompt must be visible");
    assert!(
        out.screen_seq > 0,
        "a visible live line must have moved screen_seq"
    );

    // The pending line mutates ('Enter value> ' -> 'Enter value> x') without
    // committing a line. That is a visible change and must advance screen_seq
    // again; otherwise a wait anchored on the prior seq would be satisfied
    // before the mutating output landed.
    let baseline_seq = out.screen_seq;
    let out2 = b
        .wait(WaitCond::Text("x".into()), Duration::from_secs(5))
        .expect("wait mutation");
    assert!(out2.met, "mutated pending line must be visible");
    assert!(
        out2.screen_seq > baseline_seq,
        "pending mutation must advance screen_seq ({} -> {})",
        baseline_seq,
        out2.screen_seq
    );

    b.stop().ok();
}

#[test]
fn line_cli_rejects_mouse_and_reports_exit() {
    let mut b = cli_backend("print('x')\n");
    let err = b.send_input(Input::MouseClick {
        button: tui_lab::backend::MouseButton::Left,
        x: 1,
        y: 1,
    });
    assert!(err.is_err(), "mouse must be rejected on pipes");
    // Exit surfaces through ProcessExit wait.
    let out = b
        .wait(WaitCond::ProcessExit, Duration::from_secs(5))
        .expect("wait exit");
    assert!(out.met, "short script exits");
    b.stop().ok();
}

// ── Item 52: kitty keyboard protocol ────────────────────────────────────

#[test]
fn kitty_flags_are_detected_and_super_unlocks() {
    // The app pushes kitty flags `CSI > 1 u` (progressive enhancement) and
    // prints a marker. chr(27) builds the ESC byte python-side.
    let mut b = pty_backend(
        "import sys,time\n\
         sys.stdout.write(chr(27) + '[>1u')\n\
         sys.stdout.write('KITTY-ON')\n\
         sys.stdout.flush()\n\
         time.sleep(2)",
    );
    let _ = b.wait(WaitCond::Text("KITTY-ON".into()), Duration::from_secs(5));
    let caps: Capabilities = b.capabilities();
    assert!(
        caps.kitty_keyboard,
        "kitty push must promote the capability"
    );
    // SUPER+q, rejected in legacy mode, encodes as CSI-u when active.
    let modes = b.input_modes();
    assert!(modes.kitty_flags > 0, "flags visible in input modes");
    let bytes = encode_super_q_through_trait(&mut b);
    assert!(bytes.is_ok(), "SUPER unlocks under kitty: {bytes:?}");
    b.stop().ok();
}

fn encode_super_q_through_trait(b: &mut PortablePtyBackend) -> Result<Vec<u8>, String> {
    // The trait doesn't expose raw key encoding; drive through send_input
    // and detect success by absence of an error.
    b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('q'),
        tui_lab::backend::KeyModifiers::SUPER,
    )))
    .map(|_| Vec::new())
    .map_err(|e| e.to_string())
}

#[test]
fn super_is_still_rejected_without_kitty() {
    let mut b = pty_backend("import time; time.sleep(2)");
    let err = b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('q'),
        tui_lab::backend::KeyModifiers::SUPER,
    )));
    assert!(err.is_err(), "SUPER stays rejected when no kitty push");
    b.stop().ok();
}

// ── Item 53: real scrollback + search ──────────────────────────────────

#[test]
fn pty_scrollback_captured_and_searchable() {
    let mut b = pty_backend(
        "import sys,time\n\
         print('TOP-OF-HISTORY')\n\
         for i in range(40):\n\
         \x20   print(f'hist-{i}')\n\
         \x20   sys.stdout.flush()\n\
         \x20   time.sleep(0.01)\n\
         time.sleep(2)",
    );
    let _ = b.wait(WaitCond::Text("hist-39".into()), Duration::from_secs(10));
    // Capability promoted only on evidence.
    assert!(
        b.capabilities().scrollback,
        "scrollback promoted after capture"
    );
    // Search covers viewport and scrollback.
    let hits = b.search("TOP-OF-HISTORY").expect("search");
    assert!(
        hits.iter().any(|h: &SearchHit| h.region == "scrollback"),
        "history token found in scrollback: {hits:?}"
    );
    let hits2 = b.search("hist-3").expect("search 2");
    assert!(!hits2.is_empty(), "viewport hits found");
    b.stop().ok();
}

// ── Item 56: terminal query responder ──────────────────────────────────

#[test]
fn terminal_answers_dsr_cursor_position_query() {
    // The child asks CSI 6n in raw mode and reports whether a response
    // arrived. A real terminal answers; our responder must too, or the
    // child hangs. (Raw mode matters: the default canonical line
    // discipline holds the response until a newline, which is ordinary
    // terminal behavior a real app never hits.)
    let mut b = pty_backend(
        "import sys,time,tty\n\
         fd = sys.stdin.fileno()\n\
         tty.setraw(fd)\n\
         sys.stdout.write(chr(27) + '[6n')\n\
         sys.stdout.flush()\n\
         import select\n\
         r, _, _ = select.select([sys.stdin], [], [], 5)\n\
         print('GOT-RESPONSE' if r else 'NO-RESPONSE')\n\
         sys.stdout.flush()\n\
         time.sleep(2)",
    );
    let out = b
        .wait(
            WaitCond::Text("GOT-RESPONSE".into()),
            Duration::from_secs(8),
        )
        .expect("wait");
    assert!(
        out.met,
        "child received a cursor-position response (responder works)"
    );
    b.stop().ok();
}

#[test]
fn terminal_answers_da1_query() {
    let mut b = pty_backend(
        "import sys,time,tty\n\
         fd = sys.stdin.fileno()\n\
         tty.setraw(fd)\n\
         sys.stdout.write(chr(27) + '[0c')\n\
         sys.stdout.flush()\n\
         import select\n\
         r, _, _ = select.select([sys.stdin], [], [], 5)\n\
         print('DA1-ANSWERED' if r else 'DA1-SILENT')\n\
         sys.stdout.flush()\n\
         time.sleep(2)",
    );
    let out = b
        .wait(
            WaitCond::Text("DA1-ANSWERED".into()),
            Duration::from_secs(8),
        )
        .expect("wait");
    assert!(out.met, "DA1 answered");
    b.stop().ok();
}

// ── Item 57: SVG/PNG capture ───────────────────────────────────────────

#[test]
fn capture_renders_screen_content() {
    let mut b = pty_backend("print('CAPTURE-TARGET'); import time; time.sleep(2)");
    let _ = b.wait(
        WaitCond::Text("CAPTURE-TARGET".into()),
        Duration::from_secs(5),
    );
    let st = b.state().expect("state");
    let svg = tui_lab::screen::capture::to_svg(&st);
    assert!(svg.contains("CAPTURE-TARGET"), "svg carries the text");
    let png = tui_lab::screen::capture::to_png(&st);
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    b.stop().ok();
}

// ── Items 58–63: NativeSemanticProtocol ────────────────────────────────

#[test]
fn native_channel_parses_app_frames() {
    use tui_lab::semantic::native::{parse_frame, NativeChannel};

    // Round trip through a real file the way the session does.
    let mut ch = NativeChannel::create().expect("create");
    let path = ch.path.clone().expect("path");
    std::fs::write(
        &path,
        concat!(
            r##"{"v":1,"type":"snapshot","app":"demo","framework":"raw-ansi","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save","focused":true,"enabled":false}]}}"##,
            "\n",
        ),
    )
    .expect("write");
    ch.poll();
    assert_eq!(ch.frames_accepted, 1);
    assert_eq!(ch.app.as_deref(), Some("demo"));
    let root = ch.latest.as_ref().expect("snapshot stored");
    assert_eq!(root.children[0].id, "#save");
    assert_eq!(root.children[0].focused, Some(true));
    std::fs::remove_file(&path).ok();
    // The parse fn is also public contract.
    assert!(
        parse_frame("{\"v\":1,\"type\":\"event\",\"event\":\"focus\",\"target\":\"#x\"}").is_ok()
    );
}

#[test]
fn session_injects_env_and_overlays_native_tree() {
    use tui_lab::session::state::{LaunchSpec, Session};

    // The fixture writes a native snapshot declaring focus on #cancel —
    // the OPPOSITE of what style inference would conclude first.
    let fixture = env!("CARGO_MANIFEST_DIR").to_string() + "/fixtures/nsp_tui.py";
    let mut sess = Session::new("nsp-test".into(), "python3".into());
    let spec = LaunchSpec {
        command: "python3".into(),
        args: vec![fixture],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    };
    sess.start_with_spec(spec).expect("start");
    // The env var must have been injected into the channel (and NOT stored
    // in the launch spec — the injection is per-generation, transparent).
    let path = sess.native_channel().path.clone().expect("channel created");
    assert!(path.exists(), "channel file exists");

    // Drive focus to the second button and give the app a moment to
    // declare its tree.
    sess.send(Input::Key(KeyEvent::new(KeyCode::Right)))
        .expect("focus move");
    let mut declared = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        sess.observe(40).expect("observe");
        if sess.native_channel().latest.is_some() {
            declared = true;
            break;
        }
    }
    assert!(declared, "app declared a native snapshot");

    let screen = sess.observe(40).expect("observe");
    let mut tree = tui_lab::semantic::build_tree(&screen);
    let report = sess.overlay_native(&mut tree);
    assert!(report.active(), "overlay engaged");
    assert!(
        report
            .matched
            .iter()
            .any(|id| id == "#save" || id == "#cancel"),
        "native ids matched into the tree: {:?}",
        report.matched
    );
    // Native focus wins: #cancel declared focused after the Right press.
    assert_eq!(
        report.focus_applied.as_deref(),
        Some("#cancel"),
        "app's focus declaration applied: {:?}",
        report.focus_applied
    );
    sess.stop().ok();
    std::fs::remove_file(path).ok();
}

// ── Item 64: coverage ledger ───────────────────────────────────────────

#[test]
fn coverage_ledger_accumulates_native_events() {
    use tui_lab::run::RunContext;
    use tui_lab::semantic::native::NativeChannel;

    let mut run = RunContext::ephemeral();
    let mut ch = NativeChannel::create().expect("channel");
    let path = ch.path.clone().expect("path");
    std::fs::write(
        &path,
        concat!(
            r##"{"v":1,"type":"event","event":"coverage","target":"src/lib.rs:42"}"##,
            "\n",
            r##"{"v":1,"type":"event","event":"coverage","target":"src/lib.rs:42"}"##,
            "\n",
            r##"{"v":1,"type":"event","event":"coverage","target":"widget:#save.activate"}"##,
            "\n",
        ),
    )
    .expect("write");
    ch.poll();
    // The runtime path ingests exactly once from the session event queue;
    // the test feeds the same events as a batch (the old whole-channel
    // rescan double-counted on every call).
    let folded = run.ingest_native_coverage_batch("sess-x", &ch.events_since(0));
    assert_eq!(folded, 3, "all coverage events folded");
    let ledger = &run.coverage_ledger;
    assert_eq!(ledger.len(), 2, "two distinct targets");
    assert_eq!(ledger.get("src/lib.rs:42").expect("entry").hits, 2);
    assert!(
        ledger
            .get("widget:#save.activate")
            .expect("entry")
            .sessions
            .contains(&"sess-x".to_string()),
        "session correlation kept"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn command_state_is_none_without_shell_integration() {
    let mut b = pty_backend("print('no-133-here'); import time; time.sleep(1)");
    let _ = b.wait(WaitCond::Text("no-133-here".into()), Duration::from_secs(5));
    assert!(
        b.command_state().is_none(),
        "no OSC 133 traffic → honest None"
    );
    b.stop().ok();
}

#[test]
fn command_state_tracks_osc133_edges() {
    // The child fakes shell integration: emits 133;C, then 133;D after a
    // pause. The backend must observe the phases.
    let mut b = pty_backend(
        "import sys,time\n\
         sys.stdout.write('\\x1b]133;C\\x07')\n\
         sys.stdout.write('CMD-RUNNING')\n\
         sys.stdout.flush()\n\
         time.sleep(0.6)\n\
         sys.stdout.write('\\x1b]133;D;0\\x07')\n\
         sys.stdout.write('CMD-DONE')\n\
         sys.stdout.flush()\n\
         time.sleep(2)",
    );
    let _ = b.wait(WaitCond::Text("CMD-DONE".into()), Duration::from_secs(5));
    let cs: Option<CommandState> = b.command_state();
    let cs = cs.expect("command state present after 133 traffic");
    assert_eq!(cs.command_seq, 1, "one command started");
    assert!(!cs.running, "command finished");
    assert_eq!(cs.last_exit, Some(0), "exit status parsed from 133;D;0");
    b.stop().ok();
}

// ── PipeBackend (review P0 #1): genuine pipes, stdout/stderr separation ──

/// The pipe transport is NOT a terminal: the child must see `isatty()`
/// false on stdin/stdout/stderr — the exact behavior a real `<cmd> | …`
/// redirect would produce. This is what "genuine pipe semantics" buys.
#[test]
fn pipe_child_sees_no_tty() {
    let mut b = pipe_backend(
        "import os,sys\n\
         sys.stdout.write(f'IN-{os.isatty(0)} OUT-{os.isatty(1)} ERR-{os.isatty(2)}')\n\
         sys.stdout.flush()\n\
         import time; time.sleep(1)",
    );
    let out = b
        .wait(
            WaitCond::Text("IN-False OUT-False ERR-False".into()),
            Duration::from_secs(5),
        )
        .expect("wait");
    assert!(
        out.met,
        "child must observe no TTY on any fd (all False): state={:?}",
        out.state.viewport_text
    );
    b.stop().ok();
}

/// W1b regression: two `\n`-terminated writes to the SAME stream must stay
/// two lines. The old unconditional `store.pop()` re-opened the committed
/// line, so a later chunk appended onto it ("A" + later "B" fused to "AB").
/// A mid-line split (partial write, then continuation) must still continue.
#[test]
fn pipe_committed_lines_stay_committed_across_chunks() {
    let mut b = pipe_backend(
        "import sys,time\n\
         sys.stdout.write('A\\n'); sys.stdout.flush()\n\
         time.sleep(0.15)\n\
         sys.stdout.write('B\\n'); sys.stdout.flush()\n\
         time.sleep(0.15)\n\
         sys.stdout.write('PART'); sys.stdout.flush()\n\
         time.sleep(0.15)\n\
         sys.stdout.write('UAL\\n'); sys.stdout.flush()\n\
         time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("UAL".into()), Duration::from_secs(5));
    let out = b.stdout_lines();
    assert!(out.contains(&"A".to_string()), "first line intact: {out:?}");
    assert!(
        out.contains(&"B".to_string()),
        "second line NOT fused to A: {out:?}"
    );
    assert!(
        out.contains(&"PARTUAL".to_string()),
        "genuinely unterminated line continues across chunks: {out:?}"
    );
    assert!(
        !out.iter().any(|l| l.contains("AB")),
        "committed lines must not fuse: {out:?}"
    );
    b.stop().ok();
}

/// Review P1 #28: genuine stdout/stderr separation. A child writing to both
/// file descriptors must surface each stream distinctly — the fused screen
/// interleaves them, but `stdout_lines()` / `stderr_lines()` keep them apart.
#[test]
fn pipe_separates_stdout_from_stderr() {
    let mut b = pipe_backend(
        "import sys\n\
         sys.stdout.write('OUT-APPLE\\n')\n\
         sys.stdout.flush()\n\
         sys.stderr.write('ERR-BANANA\\n')\n\
         sys.stderr.flush()\n\
         sys.stdout.write('OUT-CHERRY\\n')\n\
         sys.stdout.flush()\n\
         import time; time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("OUT-CHERRY".into()), Duration::from_secs(5));
    let out = b.stdout_lines();
    let err = b.stderr_lines();
    assert!(
        out.iter().any(|l| l.contains("OUT-APPLE")),
        "stdout: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("OUT-CHERRY")),
        "stdout: {out:?}"
    );
    assert!(
        !out.iter().any(|l| l.contains("BANANA")),
        "stdout polluted: {out:?}"
    );
    assert!(
        err.iter().any(|l| l.contains("ERR-BANANA")),
        "stderr: {err:?}"
    );
    assert!(
        !err.iter().any(|l| l.contains("APPLE")),
        "stderr polluted: {err:?}"
    );
    b.stop().ok();
}

/// The pipe screen model mirrors the line backend: output splits into the
/// (bounded) line history, the last `rows` lines render as the viewport and
/// the rest is scrollback — real and searchable, exactly like a terminal
/// consumer would see the interleaved stream.
#[test]
fn pipe_screen_has_viewport_scrollback_and_search() {
    let mut b = pipe_backend(
        "import sys,time\n\
         sys.stderr.write('TOP-STDERR\\n'); sys.stderr.flush()\n\
         for i in range(30):\n\
         \x20   print(f'pl-{i}')\n\
         \x20   sys.stdout.flush()\n\
         \x20   time.sleep(0.01)\n\
         time.sleep(1)",
    );
    let _ = b.wait(WaitCond::Text("pl-29".into()), Duration::from_secs(10));
    let st = b.state().expect("state");
    // Scrollback is real (interleaved stderr earlier + evicted stdout rows).
    assert!(
        st.scrollback.iter().any(|r| r.contains("TOP-STDERR")),
        "stderr line appears in history: {}",
        st.scrollback.len()
    );
    // Search covers history (the evicted-from-viewport early pl-* rows).
    let hits = b.search("pl-0").expect("search");
    assert!(
        hits.iter().any(|h: &SearchHit| h.region == "scrollback"),
        "{hits:?}"
    );
    // Honest capability: real history, no terminal grid features.
    let caps: Capabilities = b.capabilities();
    assert!(caps.scrollback);
    assert!(!caps.mouse && !caps.colors && !caps.title);
    b.stop().ok();
}

/// A short pipe child exits; `ProcessExit` must surface the real code.
#[test]
fn pipe_reports_exit_status() {
    let mut b = pipe_backend("import sys; sys.exit(7)\n");
    let out = b
        .wait(WaitCond::ProcessExit, Duration::from_secs(5))
        .expect("wait exit");
    assert!(out.met, "short script exits");
    let st = b.process();
    assert!(!st.running, "finished");
    assert_eq!(
        st.exit_code,
        Some(7),
        "exit code surfaced from the pipe child"
    );
    b.stop().ok();
}

/// `pipe` is selectable through the session's `make_backend`, not just the
/// trait directly — the whole MCP surface can drive a genuine-pipe session.
#[test]
fn pipe_is_selectable_backend() {
    use tui_lab::session::state::{LaunchSpec, Session};

    let mut sess = Session::new("pipe-test".into(), "python3".into());
    let spec = LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), "print('PIPE-LIVE')\n".into()],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
        backend: "pipe".into(),
        isolation: "local".into(),
    };
    sess.start_with_spec(spec).expect("start pipe session");
    let text = sess.observe(40).expect("observe").viewport_text.join("\n");
    assert!(text.contains("PIPE-LIVE"), "{text}");
    sess.stop().ok();
}

/// Fused semantic truth (re-review Wave-4): every semantic-bearing shape
/// reports the SAME focus — the app's declared focus via the native channel,
/// not an inference-only verdict that contradicts the nodes view. Proven
/// end-to-end against the real cooperative fixture: summary, semantic,
/// tree (state tree), nodes, and the flat SemanticScreen from one
/// `fused_frame()` call must all name the same focused control.
#[test]
fn fused_semantic_truth_across_all_shapes() {
    use tui_lab::session::state::{LaunchSpec, Session};

    let fixture = env!("CARGO_MANIFEST_DIR").to_string() + "/fixtures/nsp_tui.py";
    let mut sess = Session::new("nsp-fused".into(), "python3".into());
    let spec = LaunchSpec {
        command: "python3".into(),
        args: vec![fixture],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    };
    sess.start_with_spec(spec).expect("start");
    let path = sess.native_channel().path.clone().expect("channel created");

    // Drive focus to Cancel (the second button) and wait for the app to
    // declare its tree.
    sess.send(Input::Key(KeyEvent::new(KeyCode::Right)))
        .expect("focus move");
    let mut declared = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        sess.observe(40).expect("observe");
        if let Some(latest) = &sess.native_channel().latest {
            // The app declares focus in its snapshot.
            let flat = latest.flatten();
            if flat.iter().any(|(_, n)| n.focused == Some(true)) {
                declared = true;
                break;
            }
        }
    }
    assert!(declared, "app declared a native snapshot with focus");

    // One fused call: all three shapes from one detection pass + overlay.
    let (sem, tree, report) = sess.fused_frame().expect("fused frame");
    assert!(report.active(), "overlay engaged");
    let flat_focus = sem.focus.control.clone();
    assert!(
        flat_focus.as_deref().is_some_and(|f| f.contains("Cancel")),
        "flat focus is the app's declaration: {flat_focus:?}"
    );
    assert_eq!(sem.focus.confidence, 1.0, "native focus is confidence 1.0");

    // The nodes tree agrees: exactly one focused node, and it is Cancel.
    let focused_nodes: Vec<String> = {
        fn walk(n: &tui_lab::semantic::node::SemanticNode, out: &mut Vec<String>) {
            if n.state.focused {
                out.push(n.label.clone().unwrap_or_else(|| n.id.clone()));
            }
            for c in &n.children {
                walk(c, out);
            }
        }
        let mut v = Vec::new();
        walk(&tree.root, &mut v);
        v
    };
    assert_eq!(
        focused_nodes.len(),
        1,
        "exactly one focused tree node: {focused_nodes:?}"
    );
    assert!(
        focused_nodes[0].contains("Cancel"),
        "tree focus matches: {focused_nodes:?}"
    );

    // The flat controls agree with the tree on which control is focused:
    // same detection pass, same overlay — impossible to diverge.
    let focused_controls: Vec<&str> = sem
        .controls
        .iter()
        .filter(|c| c.focused)
        .map(|c| c.label.as_str())
        .collect();
    assert!(
        focused_controls.iter().any(|l| l.contains("Cancel")),
        "flat focused controls: {focused_controls:?}"
    );

    // And a second call is cache-served (structure unchanged) while the
    // overlay still applies — the fused path is idempotent and never freezes
    // the native facts.
    let (sem2, tree2, report2) = sess.fused_frame().expect("second fused frame");
    assert_eq!(
        sem2.focus.control, sem.focus.control,
        "stable focus across reads"
    );
    assert_eq!(report2.focus_applied, report.focus_applied);
    let focused2: Vec<String> = {
        fn walk2(n: &tui_lab::semantic::node::SemanticNode, out: &mut Vec<String>) {
            if n.state.focused {
                out.push(n.label.clone().unwrap_or_else(|| n.id.clone()));
            }
            for c in &n.children {
                walk2(c, out);
            }
        }
        let mut v = Vec::new();
        walk2(&tree2.root, &mut v);
        v
    };
    assert_eq!(focused2, focused_nodes, "tree focus stable across reads");

    sess.stop().ok();
    std::fs::remove_file(path).ok();
}

// ─── Wave 6: durability/performance (items 49–52) ────────────────────────

/// Item 49: the audit transaction reports its own timing — driver/verify/
/// total milliseconds ride on the last finding's evidence, and a clean run
/// still produces the AUDIT-METRICS info finding so the numbers never
/// vanish.
#[test]
fn audit_transaction_reports_timing_metrics() {
    let mut s = tui_lab::session::Session::new("w6-metrics".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), "print('w6'); input()".into()],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("start");

    // A driver that finds something: metrics attach to the last finding.
    let findings = tui_lab::audit::transaction::run_verified(&mut s, "probe", |_sess| {
        vec![tui_lab::audit::Finding {
            id: "PROBE-1".into(),
            rule_id: None,
            severity: "info".into(),
            category: "probe".into(),
            summary: "probe hit".into(),
            evidence: vec![tui_lab::audit::EvidenceRef::point(
                tui_lab::audit::EvidenceKind::Other,
                "probe/target",
                "hit",
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        }]
    })
    .expect("run");
    assert_eq!(findings.len(), 1);
    let m = findings[0].evidence[0].detail["audit_metrics"].clone();
    assert_eq!(m["profile"], "probe");
    assert!(m["total_ms"].is_u64(), "timing present on the finding: {m}");
    assert!(
        m["total_ms"].as_u64().unwrap() >= m["driver_ms"].as_u64().unwrap_or(0),
        "total covers the driver phase: {m}"
    );

    // A clean driver: the AUDIT-METRICS info finding carries the numbers.
    let clean = tui_lab::audit::transaction::run_verified(&mut s, "noop", |_| Vec::new())
        .expect("clean run");
    assert_eq!(clean.len(), 1, "exactly the metrics finding");
    assert_eq!(clean[0].id, "AUDIT-METRICS");
    assert_eq!(clean[0].severity, "info");
    let cm = clean[0].evidence[0].detail["audit_metrics"].clone();
    assert_eq!(cm["profile"], "noop");
    assert!(cm["verify_ms"].is_u64(), "verify phase timed: {cm}");
    s.stop().ok();
}

/// Item 51: committed frames land in a bounded hot ring queryable by
/// citable id — ids, hashes, seqs, semantic identity — with declared
/// eviction past the bound. The full grid stays cold.
#[test]
fn frame_records_are_hot_queryable_and_bounded() {
    let mut run = tui_lab::run::RunContext::ephemeral();
    let mut f1 =
        tui_lab::backend::CanonicalFrame::new(tui_lab::screen::ScreenState::new(80, 24), 1, 10);
    f1.state.structure_hash = "w6-a".into();
    f1.state.visual_hash = "v-w6-a".into();
    let id1 = run.commit_frame(&mut f1, Some("w6-sess")).expect("commit");
    let mut f2 =
        tui_lab::backend::CanonicalFrame::new(tui_lab::screen::ScreenState::new(80, 24), 2, 11);
    f2.state.structure_hash = "w6-b".into();
    let id2 = run.commit_frame(&mut f2, Some("w6-sess")).expect("commit");

    let r1 = run.frame_record(id1).expect("hot record for frame 1");
    assert_eq!(r1.session.as_deref(), Some("w6-sess"));
    assert_eq!(r1.structure_hash, "w6-a");
    assert_eq!(r1.visual_hash, "v-w6-a");
    assert_eq!(r1.screen_seq, 1);
    assert!(
        !r1.semantic_identity.is_empty(),
        "semantic identity stamped"
    );
    assert!(r1.commit_us <= 1_000_000, "commit timing plausible");
    assert_eq!(run.frame_record(id2).expect("frame 2").screen_seq, 2);
    assert_eq!(run.frame_hot_records().count(), 2, "both resident");

    // Overflow: eviction is declared, never silent.
    for i in 0..(tui_lab::run::FRAME_HOT_RING as u64 + 10) {
        let mut f = tui_lab::backend::CanonicalFrame::new(
            tui_lab::screen::ScreenState::new(80, 24),
            100 + i,
            0,
        );
        f.state.structure_hash = format!("w6-overflow-{i}");
        run.commit_frame(&mut f, None).expect("commit");
    }
    assert!(
        run.frame_hot_evicted() >= 10,
        "eviction counted: {}",
        run.frame_hot_evicted()
    );
    assert!(
        run.frame_record(id1).is_none(),
        "oldest record left the hot ring"
    );
    let status = run.status(Vec::new());
    assert_eq!(
        status["frames"]["hot_evicted"],
        serde_json::json!(run.frame_hot_evicted())
    );
    assert!(
        status["frames"]["hot_resident"].as_u64().unwrap() <= tui_lab::run::FRAME_HOT_RING as u64
    );
}

/// Item 52: the fused commit is reactive — an unchanged frame identity is
/// served from the memo (hits rise, no recompute), and any invalidating
/// input (new frame, fresh native events) recomputes correctly.
#[test]
fn fused_commit_is_reactive_and_correctly_invalidated() {
    use tui_lab::session::state::{LaunchSpec, Session};
    let fixture = env!("CARGO_MANIFEST_DIR").to_string() + "/fixtures/nsp_tui.py";
    let mut sess = Session::new("w6-reactive".into(), "python3".into());
    sess.start_with_spec(LaunchSpec {
        command: "python3".into(),
        args: vec![fixture],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("start");
    sess.observe(60).expect("first observe");
    let hits_before = sess.fused_memo_hits();

    // Same frame identity: repeated fused reads are memo-served.
    let (sem1, tree1, report1) = sess.fused_frame().expect("fused 1");
    let (sem2, tree2, report2) = sess.fused_frame().expect("fused 2");
    assert_eq!(sem2.focus.control, sem1.focus.control);
    assert_eq!(tree2.root.id, tree1.root.id);
    assert_eq!(report2.native_ids, report1.native_ids);
    assert!(
        sess.fused_memo_hits() > hits_before,
        "memo served repeat reads: {} -> {}",
        hits_before,
        sess.fused_memo_hits()
    );

    // Invalidate via explicit hook, and confirm recompute gives the same
    // answer (correctness of the reactive path, not a stale memo).
    sess.invalidate_fused();
    let hits_mid = sess.fused_memo_hits();
    let (sem3, _t3, r3) = sess.fused_frame().expect("fused 3");
    assert_eq!(
        sem3.focus.control, sem1.focus.control,
        "same truth after recompute"
    );
    assert_eq!(r3.native_ids, report1.native_ids);
    assert_eq!(
        sess.fused_memo_hits(),
        hits_mid,
        "recompute did not count as a hit"
    );

    // A new frame (focus move changes the app's declared tree) invalidates
    // through the key: the memo must not serve stale facts.
    sess.send(tui_lab::backend::Input::Key(
        tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Right),
    ))
    .expect("focus move");
    sess.observe(150).expect("observe after move");
    let (sem4, _t4, _r4) = sess.fused_frame().expect("fused 4");
    assert!(
        sess.fused_memo_hits() == hits_mid,
        "a new frame is a new identity — no memo hit for it"
    );
    let _ = sem4;

    sess.stop().ok();
    std::fs::remove_file(sess.native_channel().path.clone().unwrap()).ok();
}
