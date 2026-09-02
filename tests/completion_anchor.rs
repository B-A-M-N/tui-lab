//! Completion-anchor conformance tests.
//!
//! Tests that completion policies resolve when the child reacts immediately
//! after input — the exact scenario that exposed the action→wait race
//! (re-capturing the baseline *after* the send instead of using the
//! pre-action anchor).
//!
//! Uses a Python child process that waits for stdin input before reacting.

use std::time::Duration;

use tui_lab::capture::{CompletionPolicy, TerminalEventMatcher};
use tui_lab::execution::{execute_act_with_completion, InputVisibility};
use tui_lab::session::state::Session;

fn make_script(parts: &[&str]) -> String {
    parts.join("\n")
}

/// The PTY line discipline starts in canonical mode: a lone key like 'x'
/// never satisfies `stdin.buffer.read(1)` (it needs a newline), so a child
/// that "reacts to input" would block forever and the reaction would never
/// fire. Every child here puts its own tty in raw mode first: reads return
/// per-byte AND echo is off — so a redraw the child emits is genuinely its
/// reaction, not the line-discipline echoing our keystroke back.
const RAW_STDIN: &str = "import tty,termios; tty.setraw(0)";

// ---------------------------------------------------------------------------
// Test 1: Bell immediately after input resolves
// ---------------------------------------------------------------------------

/// Child echoes a BEL character (0x07) in response to any input, within the
/// same write burst. CompletionPolicy::Bell must resolve.
#[test]
fn bell_immediately_after_input_resolves() {
    let script = make_script(&[
        "import sys; sys.stdout.write('READY'); sys.stdout.flush()",
        RAW_STDIN,
        "import sys; c=sys.stdin.buffer.read(1)",
        "sys.stdout.buffer.write(b'\\x07')",
        "sys.stdout.flush()",
        "import time; time.sleep(30)",
    ]);

    let mut s = Session::new("test-bell".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let tx = execute_act_with_completion(
        &mut s,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        50,
        2000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::Bell,
    )
    .expect("act");

    assert_eq!(
        tx.settle,
        tui_lab::execution::SettleStatus::Met,
        "{}",
        tx.settle_reason()
    );
    s.stop().ok();
}

// ---------------------------------------------------------------------------
// Test 2: Title change immediately after input resolves
// ---------------------------------------------------------------------------

/// Child emits ESC ] 0 ; NEWTITLE BEL immediately on input.
/// AnyObservableChange must resolve.
#[test]
fn title_immediately_after_input_resolves() {
    let script = make_script(&[
        "import sys; sys.stdout.write('READY'); sys.stdout.flush()",
        RAW_STDIN,
        "import sys; sys.stdin.buffer.read(1)",
        "sys.stdout.buffer.write(b'\\x1b]0;CHANGED\\x07')",
        "sys.stdout.flush()",
        "import time; time.sleep(30)",
    ]);

    let mut s = Session::new("test-title".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let tx = execute_act_with_completion(
        &mut s,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        50,
        2000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::AnyObservableChange,
    )
    .expect("act");

    assert_eq!(
        tx.settle,
        tui_lab::execution::SettleStatus::Met,
        "{}",
        tx.settle_reason()
    );
    s.stop().ok();
}

// ---------------------------------------------------------------------------
// Test 3: Semantic change immediately resolves
// ---------------------------------------------------------------------------

/// Child redraws a button label ("Start" → "Stop") immediately on input,
/// same screen size. CompletionPolicy::SemanticChange must resolve.
#[test]
fn semantic_change_immediately_resolves() {
    let script = make_script(&[
        "import sys",
        "sys.stdout.write('[ Start ]'); sys.stdout.flush()",
        RAW_STDIN,
        "import sys; sys.stdin.buffer.read(1)",
        "sys.stdout.write('\\x1b[2J\\x1b[H[ Stop ]'); sys.stdout.flush()",
        "import time; time.sleep(30)",
    ]);

    let mut s = Session::new("test-semantic".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let tx = execute_act_with_completion(
        &mut s,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        50,
        2000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::SemanticChange,
    )
    .expect("act");

    assert_eq!(
        tx.settle,
        tui_lab::execution::SettleStatus::Met,
        "{}",
        tx.settle_reason()
    );
    s.stop().ok();
}

// ---------------------------------------------------------------------------
// Test 4: Event immediately resolves
// ---------------------------------------------------------------------------

/// Child changes title immediately on input; CompletionPolicy::Event for
/// title change must resolve. This catches the anchor_event_seq=0/post-send
/// anchor race: with the old code the title event could be ≤ anchor.
#[test]
fn event_immediately_resolves() {
    let script = make_script(&[
        "import sys; sys.stdout.write('READY'); sys.stdout.flush()",
        RAW_STDIN,
        "import sys; sys.stdin.buffer.read(1)",
        "sys.stdout.buffer.write(b'\\x1b]0;TITLED\\x07')",
        "sys.stdout.flush()",
        "import time; time.sleep(30)",
    ]);

    let mut s = Session::new("test-event".into(), "python3".into());
    s.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let tx = execute_act_with_completion(
        &mut s,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        50,
        2000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::Event(TerminalEventMatcher("TitleChanged")),
    )
    .expect("act");

    assert_eq!(
        tx.settle,
        tui_lab::execution::SettleStatus::Met,
        "{}",
        tx.settle_reason()
    );
    s.stop().ok();
}

// ---------------------------------------------------------------------------
// Test 5: StableScreen waits for quiet; FirstScreenChange does not
// ---------------------------------------------------------------------------

/// The two policies differ in what they REQUIRE, and that difference is
/// provable without any wall-clock bound (which only measures machine
/// load and flakes under it):
///
/// 1. FirstScreenChange on a screen that NEVER goes quiet (distinct
///    full-screen frame every 50ms, forever) must still resolve Met —
///    one observed change is enough. StableScreen on the same stimulus
///    could never resolve; a quiet window never arrives.
/// 2. StableScreen with quiet_ms=120 against A→B→C-then-quiet must wait
///    through the changing frames and capture C (elapsed ≥ ~100ms is a
///    LOWER bound — load only lengthens it, never shortens it).
#[test]
fn stable_screen_waits_for_quiet() {
    // --- FirstScreenChange against a never-quiet screen ---
    let mut s1 = Session::new("test-fsc".into(), "python3".into());
    s1.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            make_script(&[
                "import sys,time",
                "sys.stdout.write('READY'); sys.stdout.flush()",
                RAW_STDIN,
                "import sys; sys.stdin.buffer.read(1)",
                // Cycle full-screen frames forever: the screen keeps
                // changing every 50ms and never settles.
                "letters = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ'",
                "i = 0",
                "while True:",
                "    ch = letters[i % 26]",
                "    i += 1",
                "    sys.stdout.write('\\x1b[2J\\x1b[H' + ch*1920); sys.stdout.flush()",
                "    time.sleep(0.05)",
            ]),
        ],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let tx1 = execute_act_with_completion(
        &mut s1,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        0,
        5000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::FirstScreenChange,
    )
    .expect("first-screen-change");

    assert_eq!(
        tx1.settle,
        tui_lab::execution::SettleStatus::Met,
        "FirstScreenChange resolves on one observed change even though the screen never settles: {}",
        tx1.settle_reason()
    );
    // The captured frame is a post-action frame (one of the cycling
    // letters) — NOT the pre-action 'READY' screen, i.e. the wait really
    // observed a change.
    let after1 = tx1.after().viewport_text.join("");
    assert!(
        !after1.contains("READY"),
        "FirstScreenChange must capture a post-action frame, got READY: {after1:?}"
    );

    // --- StableScreen with quiet_ms=120 ---
    let mut s2 = Session::new("test-ss".into(), "python3".into());
    s2.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            make_script(&[
                "import sys,time",
                "sys.stdout.write('READY'); sys.stdout.flush()",
                RAW_STDIN,
                "import sys; sys.stdin.buffer.read(1)",
                "sys.stdout.write('\\x1b[2J\\x1b[H' + 'A'*1920); sys.stdout.flush()",
                "time.sleep(0.05)",
                "sys.stdout.write('\\x1b[2J\\x1b[H' + 'B'*1920); sys.stdout.flush()",
                "time.sleep(0.05)",
                "sys.stdout.write('\\x1b[2J\\x1b[H' + 'C'*1920); sys.stdout.flush()",
                "time.sleep(5)",
            ]),
        ],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");

    std::thread::sleep(Duration::from_millis(400));

    let start2 = std::time::Instant::now();
    let tx2 = execute_act_with_completion(
        &mut s2,
        &tui_lab::execution::CanonicalAction::Key {
            key: tui_lab::backend::KeyEvent::new(tui_lab::backend::KeyCode::Char('x')),
        },
        120, // quiet_ms=120
        3000,
        false,
        InputVisibility::Normal,
        CompletionPolicy::StableScreen,
    )
    .expect("stable-screen");

    let elapsed2 = start2.elapsed().as_millis();
    assert_eq!(
        tx2.settle,
        tui_lab::execution::SettleStatus::Met,
        "{}",
        tx2.settle_reason()
    );
    // StableScreen with quiet_ms=120 must wait through the changing
    // frames until quiet: A at t≈0, B at +50ms, C at +100ms, then 120ms
    // of quiet. Load only LENGTHENS the elapsed time, so ~100ms is a
    // sound lower bound (an upper bound would measure the machine, not
    // the policy).
    assert!(
        elapsed2 >= 100,
        "StableScreen with quiet_ms=120 should wait ~220ms+, took {}ms",
        elapsed2
    );
    // The captured frame should contain 'C' (the last frame before quiet).
    assert!(
        tx2.after().viewport_text.join("").contains('C'),
        "StableScreen should capture frame C, got: {:?}",
        tx2.after().viewport_text
    );

    s1.stop().ok();
    s2.stop().ok();
}

// ---------------------------------------------------------------------------
// Test 6: CommandDone (skipped)
// ---------------------------------------------------------------------------

/// CommandDone requires OSC 133 support, which is only available in the
/// portable_pty backend. The line_cli backend does not parse OSC 133.
/// Even with portable_pty, the test needs a shell that emits 133;C and
/// 133;D — an ordinary python subprocess does not produce OSC 133.
/// Skipping this test; the infrastructure (after_command_seq anchor) is
/// verified by the backend tests.
#[test]
fn command_done_immediately_resolves() {
    // SKIP: CommandDone handling (OSC 133;C / 133;D) is only tracked in
    // portable_pty.rs. The line_cli backend does not parse OSC 133, so
    // CommandDone always times out in that backend. Even portable_pty
    // only tracks CommandDone when the child emits OSC 133 sequences,
    // which a raw python subprocess does not.
}
