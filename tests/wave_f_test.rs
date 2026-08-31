//! Wave F integration suite (items 50–64): borrowed-system integrations
//! proven against real python children through the real backends.
//!
//! Every test exercises the trait boundary or the MCP session path — no
//! mocks — because these features are only real when a live process
//! negotiates them.

use std::time::Duration;

use tui_lab::backend::line_cli::LineCliBackend;
use tui_lab::backend::portable_pty::PortablePtyBackend;
use tui_lab::backend::{
    Capabilities, CommandState, Input, KeyCode, KeyEvent, SearchHit, TerminalBackend, WaitCond,
};

/// Spawn a child that prints lines slowly (CLI-style output).
fn cli_backend(script: &str) -> LineCliBackend {
    let mut b = LineCliBackend::new(80, 24);
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

// ── Items 50–51: LineCliBackend ─────────────────────────────────────────

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
    let _ = b.wait(
        WaitCond::Text("line-29".into()),
        Duration::from_secs(10),
    );
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
    assert!(caps.kitty_keyboard, "kitty push must promote the capability");
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
        .wait(WaitCond::Text("GOT-RESPONSE".into()), Duration::from_secs(8))
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
        .wait(WaitCond::Text("DA1-ANSWERED".into()), Duration::from_secs(8))
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
    assert!(parse_frame("{\"v\":1,\"type\":\"event\",\"event\":\"focus\",\"target\":\"#x\"}").is_ok());
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
    let path = sess
        .native_channel()
        .path
        .clone()
        .expect("channel created");
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
        report.matched.iter().any(|id| id == "#save" || id == "#cancel"),
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
    let folded = run.collect_native_coverage("sess-x", &ch);
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
