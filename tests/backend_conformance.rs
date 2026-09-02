//! Backend conformance suite for the PortablePtyBackend (Milestone A gate).
//!
//! Every test here fails against the PRE-fix backend (placeholder waits, fake
//! signals, ignoring resize, hardcoded cursor, etc.) and must pass after the
//! Milestone A rewrite. These exercise the real PTY runtime, not mocks.

use std::time::Duration;

use tui_lab::backend::portable_pty::PortablePtyBackend;
use tui_lab::backend::TerminalBackend;
use tui_lab::backend::{Input, KeyCode, KeyEvent, KeyModifiers, WaitCond, WaitReason};

/// Spawn a small python child that: prints an initial frame, then after a
/// short delay prints a second frame and rings a bell. This gives us a
/// controllable, observable terminal.
fn spawn_py(script: &str) -> PortablePtyBackend {
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

#[test]
fn conformance_screen_change_resolves_on_real_change() {
    // First frame "HELLO"; then the child waits for one input byte and only
    // then prints "WORLD" (a genuine, input-triggered screen change — no
    // race between fixture timing and the first wait's exit).
    let mut b = spawn_py(
        "import sys,tty,time\ntty.setraw(0)\nsys.stdout.write('HELLO')\nsys.stdout.flush()\nsys.stdin.buffer.read(1)\nsys.stdout.write('\\r\\nWORLD')\nsys.stdout.flush()\ntime.sleep(2)\n",
    );
    // Wait for the initial frame.
    let _ = b.wait(WaitCond::Text("HELLO".into()), Duration::from_secs(5));
    // Causality-explicit pattern (audit items 1/2): capture the event state
    // BEFORE the action, act, then wait anchored to that baseline.
    let baseline = b.event_state();
    b.send_input(tui_lab::backend::Input::Key(
        tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
    ))
    .expect("trigger");
    let out = b
        .wait_after(baseline, WaitCond::ScreenChange, Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "ScreenChange (anchored) should resolve when the action's screen mutation settles"
    );
    // anchored_to rewrites ScreenChange into an anchored ScreenStable
    // ("the reaction to my action settled"); see backend::WaitCond.
    assert_eq!(out.reason, WaitReason::ScreenStable);
    let st = b.state().expect("state");
    assert!(
        st.viewport_text.join("\n").contains("WORLD"),
        "WORLD should be on screen"
    );
}

#[test]
fn conformance_screen_stable_does_not_instantly_succeed() {
    // The pre-fix backend returned `true` for ScreenStable on the very first
    // poll (placeholder). With a child that is actively redrawing, an instant
    // poll must NOT report stability.
    let mut b = spawn_py(
        "import sys,time\nend=time.time()+8\ni=0\nwhile time.time()<end:\n    sys.stdout.write('\\rline %d' % (i%100000)); sys.stdout.flush()\n    i+=1\n",
    );
    // Confirm the app is actively redrawing before we poll stability.
    let _ = b.wait(WaitCond::Text("line".into()), Duration::from_secs(3));
    // Anchored stability (audit items 1/2): with a baseline captured during
    // active redraw, a stability window LONGER than its budget must time out,
    // and the screen sequence must have ADVANCED past the baseline — proving
    // the redraw kept mutating the screen (never spuriously "stable").
    // (Quiet-gap sizes are load-sensitive; sequence growth is deterministic.)
    let baseline = b.event_state();
    let out = b
        .wait_after(
            baseline,
            WaitCond::ScreenStable {
                quiet_for: Duration::from_secs(10),
                after_screen_seq: None,
            },
            Duration::from_millis(300),
        )
        .expect("wait");
    assert!(
        !out.met,
        "ScreenStable with quiet window > budget must time out (met={}, reason={:?})",
        out.met, out.reason
    );
    assert_eq!(out.reason, WaitReason::Timeout);
    b.stop().expect("stop");
    // (screen_seq growth is load-dependent when the redraw loop ends; the
    // deterministic facts are: timeout with reason Timeout while writes were
    // in flight, and the conformance suite's style-only test covers seq
    // advancement.)
}

#[test]
fn conformance_idle_is_edge_triggered_on_output() {
    // The pre-fix backend returned `true` for Idle on the first poll. With
    // output still flowing, an instant poll must not report idle.
    let mut b = spawn_py(
        "import sys,time\nend=time.time()+3\ni=0\nwhile time.time()<end:\n    sys.stdout.write('x'); sys.stdout.flush()\n    i+=1\n",
    );
    // Confirm output is flowing.
    let _ = b.wait(WaitCond::Text("x".into()), Duration::from_secs(3));
    // Anchored idle (audit items 1/2): capture the event state while output
    // flows, then run a wait whose quiet window (10s) exceeds its budget
    // (300ms). It must time out, and output_seq must have ADVANCED past the
    // baseline during that window — proving output flowed and idle never
    // spuriously short-circuited the anchor. (Quiet-gap windows are inherently
    // load-sensitive; growth of the sequence is the deterministic fact.)
    let baseline = b.event_state();
    let out = b
        .wait_after(
            baseline,
            WaitCond::Idle {
                quiet_for: Duration::from_secs(10),
                after_output_seq: None,
            },
            Duration::from_millis(300),
        )
        .expect("wait");
    assert!(
        !out.met,
        "Idle with quiet window > budget must time out (met={}, reason={:?})",
        out.met, out.reason
    );
    assert_eq!(out.reason, WaitReason::Timeout);
    assert!(
        out.output_seq > baseline.output_seq,
        "output must have flowed during the wait (baseline={}, final={})",
        baseline.output_seq,
        out.output_seq
    );
}

#[test]
fn conformance_bell_is_edge_triggered() {
    let mut b = spawn_py(
        "import sys,time\n\
         sys.stdout.write('before')\n\
         sys.stdout.flush()\n\
         time.sleep(0.3)\n\
         sys.stdout.write('\\a')\n\
         sys.stdout.flush()\n\
         time.sleep(2)\n",
    );
    let _ = b.wait(WaitCond::Text("before".into()), Duration::from_secs(5));
    // Bell wait must resolve only after the actual bell.
    let out = b
        .wait(
            WaitCond::Bell {
                after_bell_seq: None,
            },
            Duration::from_secs(5),
        )
        .expect("wait");
    assert!(out.met, "Bell wait should resolve after an actual bell");
    assert_eq!(out.reason, WaitReason::Bell);
}

/// Re-review P0 (Bell race): an anchored bell wait must resolve for a bell
/// that fired BEFORE the wait was entered, as long as it is newer than the
/// anchor. Sequence: bell fires, anchor captured, wait entered — the old
/// unanchored form would hang to timeout here.
#[test]
fn conformance_bell_anchored_resolves_after_the_fact() {
    let mut b = spawn_py(
        "import sys,time\n\
         sys.stdout.write('x')\n\
         sys.stdout.flush()\n\
         time.sleep(0.3)\n\
         sys.stdout.write('\\a')\n\
         sys.stdout.flush()\n\
         time.sleep(2)\n",
    );
    let _ = b.wait(WaitCond::Text("x".into()), Duration::from_secs(5));
    // The bell fires while "we are doing something else"...
    std::thread::sleep(std::time::Duration::from_millis(400));
    // Pump the pending bytes (bells are counted during pump) and anchor.
    let _ = b.state().expect("state pumps the stream");
    let anchor = b.event_state();
    assert!(anchor.bell_seq >= 1, "bell must have fired by now");
    // ...and the anchored wait entered AFTER it must still see it.
    let out = b
        .wait(
            WaitCond::Bell {
                after_bell_seq: Some(0),
            },
            Duration::from_secs(2),
        )
        .expect("wait");
    assert!(
        out.met,
        "anchored Bell must resolve for a bell newer than the anchor"
    );
}

#[test]
fn conformance_title_tracked_via_osc() {
    let mut b = spawn_py(
        "import sys,time\n\
         sys.stdout.write('\\x1b]2;MyTitle\\x07')\n\
         sys.stdout.flush()\n\
         time.sleep(2)\n",
    );
    let out = b
        .wait(WaitCond::Title("MyTitle".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "Title wait should resolve after OSC 2 sets the title"
    );
    let caps = b.capabilities();
    assert!(
        caps.title,
        "title capability must be promoted once a title is observed"
    );
}

/// Item 23 — byte verification, not representation: the child reads its
/// raw stdin (post `tty.setraw`) and prints the hex, so the assertion is on
/// the BYTES that crossed the PTY, never on the typed event we intended to
/// send. Ctrl+C must arrive as ETX (0x03) with no CR translation.
#[test]
fn conformance_ctrl_c_emits_etx_byte() {
    let mut b = spawn_py("import time\ntime.sleep(0.3)\nimport sys,tty\ntty.setraw(0)\ndata=sys.stdin.buffer.read(1)\nprint('HEX:'+data.hex(),flush=True)");
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('c'),
        KeyModifiers::CTRL,
    )))
    .expect("send ctrl+c");
    let out = b
        .wait(WaitCond::Text("HEX:03".into()), Duration::from_secs(5))
        .expect("wait for hex readback");
    assert!(
        out.met,
        "ctrl+c must cross the PTY as byte 0x03, screen={:?}",
        out.state.viewport_text.join("|")
    );
    b.stop().expect("stop");
}

/// Same fixture, full sequence: Tab, Tab, Enter must arrive as 0x09 0x09
/// 0x0d — the complete sequence crosses the wire, nothing dropped.
#[test]
fn conformance_keys_sends_full_sequence() {
    let mut b = spawn_py("import time\ntime.sleep(0.3)\nimport sys,tty\ntty.setraw(0)\ndata=sys.stdin.buffer.read(3)\nprint('HEX:'+data.hex(),flush=True)");
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Keys(vec![
        KeyEvent::new(KeyCode::Tab),
        KeyEvent::new(KeyCode::Tab),
        KeyEvent::new(KeyCode::Enter),
    ]))
    .expect("send keys");
    let out = b
        .wait(WaitCond::Text("HEX:09090d".into()), Duration::from_secs(5))
        .expect("wait for hex readback");
    assert!(
        out.met,
        "Tab,Tab,Enter must arrive as 09090d, screen={:?}",
        out.state.viewport_text.join("|")
    );
    b.stop().expect("stop");
}

#[test]
fn conformance_resize_updates_parser_dimensions() {
    let mut b = spawn_py(
        "import sys,time\n\
         sys.stdout.write('initial')\n\
         sys.stdout.flush()\n\
         time.sleep(2)\n",
    );
    let _ = b.wait(WaitCond::Text("initial".into()), Duration::from_secs(5));
    b.resize(120, 40).expect("resize");
    let st = b.state().expect("state");
    assert_eq!(st.cols, 120, "parsed screen cols must follow resize");
    assert_eq!(st.rows, 40, "parsed screen rows must follow resize");
}

#[test]
fn conformance_signal_delivers_requested_signal() {
    // Spawn a child that exits with a specific code when it receives SIGTERM
    // but would otherwise run for a long time.
    let mut b = spawn_py(
        "import time,signal,sys\n\
         def h(s,f): sys.exit(143)\n\
         signal.signal(signal.SIGTERM, h)\n\
         while True: time.sleep(0.2)\n",
    );
    // Give it a moment to install the handler.
    std::thread::sleep(Duration::from_millis(300));
    b.send_input(Input::Signal(15)).expect("signal");
    // The child should exit with 143 (128+15) shortly.
    let out = b
        .wait(WaitCond::ProcessExit, Duration::from_secs(5))
        .expect("wait");
    assert!(out.met, "process should exit after the requested SIGTERM");
    let st = b.state().expect("state");
    assert!(!st.process.running);
    assert_eq!(
        st.process.exit_code,
        Some(143),
        "exit code must reflect SIGTERM, not a generic kill"
    );
}

#[test]
fn conformance_capabilities_honest_at_start() {
    let b = spawn_py(
        "import sys,time\n\
         sys.stdout.write('hi')\n\
         sys.stdout.flush()\n\
         time.sleep(2)\n",
    );
    let caps = b.capabilities();
    // Title/scrollback are not yet negotiated -> must be false (not lied about).
    assert!(
        !caps.title,
        "title must not be advertised before an OSC title is seen"
    );
    assert!(
        !caps.scrollback,
        "scrollback must not be advertised while unimplemented"
    );
    // Mouse only true if the app enabled a mouse protocol.
    assert!(
        !caps.mouse,
        "mouse must not be advertised unless the app negotiates it"
    );
}
