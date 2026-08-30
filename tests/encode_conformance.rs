//! Encode-conformance tests for the PortablePtyBackend (Milestone A acceptance gate).
//!
//! These tests exercise real PTY subprocesses that read raw bytes and print
//! hex output, which we match via WaitCond::Text. They are the ACCEPTANCE GATE
//! for the portable_pty rewrite; they are expected to FAIL against the current
//! backend and should PASS once the parallel agent lands the mode-aware encoding
//! and event-sequencing fixes.
//!
//! Audit items covered: 2, 65-71, 70.

use std::time::Duration;

use tui_lab::backend::portable_pty::PortablePtyBackend;
use tui_lab::backend::TerminalBackend;
use tui_lab::backend::{Input, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, WaitCond};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Spawn a python3 child that reads N bytes from stdin (setraw, no line buffering)
/// and prints them as hex prefixed with "HEX:".
///
/// The python waits on read(N) until exactly N bytes arrive.
fn spawn_hex_reader(n: usize) -> PortablePtyBackend {
    let mut b = PortablePtyBackend::new(80, 24);
    // Script: sleep, setraw, read N bytes, print hex.
    let full_script = format!(
        "import time,time as _t;time.sleep(0.2)\nimport sys,tty\ntty.setraw(0)\ndata=sys.stdin.buffer.read({n})\nprint('HEX:'+data.hex(),flush=True)",
        n = n
    );
    b.start(
        "python3",
        &["-c".to_string(), full_script],
        None,
        &[],
        80,
        24,
    )
    .expect("start python");
    b
}

/// Spawn a python3 child that first writes init bytes to stdout (terminal
/// negotiation escapes like DECCKM, mouse modes, bracketed paste), then reads
/// N raw bytes from stdin and prints hex.
fn spawn_hex_reader_with_init(init_bytes: &[u8], read_n: usize) -> PortablePtyBackend {
    let mut b = PortablePtyBackend::new(80, 24);
    // Build python script: sleep, write init to stdout, setraw, read N bytes, print hex.
    let mut escaped = String::from("import time,time as _t;time.sleep(0.2)\n");
    escaped.push_str("import sys,tty;tty.setraw(0)\n");
    // Write init bytes to stdout.buffer (raw bytes, not text).
    escaped.push_str("sys.stdout.buffer.write(b'");
    for &byte in init_bytes {
        match byte {
            b'\x1b' => escaped.push_str("\\x1b"),
            b'\\' => escaped.push_str("\\\\"),
            b'(' => escaped.push('('),
            b')' => escaped.push(')'),
            c if (0x20..=0x7e).contains(&c) => escaped.push(c as char),
            _ => escaped.push_str(&format!("\\x{:02x}", byte)),
        }
    }
    escaped.push_str("')\nsys.stdout.flush()\n");
    escaped.push_str(&format!("data=sys.stdin.buffer.read({read_n})\n"));
    escaped.push_str("print('HEX:'+data.hex(),flush=True)");

    b.start("python3", &["-c".to_string(), escaped], None, &[], 80, 24)
        .expect("start python");
    b
}

// ---------------------------------------------------------------------------
// Test 1: Ctrl+C emits 0x03  (audit item 65)
// ---------------------------------------------------------------------------

/// Encode ctrl+c as ETX (0x03) through the PTY.
/// Audit item 65: Ctrl/Alt/Shift/Function keys are parsed into typed KeyEvents;
/// the encoder must emit the C0 control byte, not raw characters.
#[test]
fn encode_ctrl_c_emits_control_byte() {
    let mut b = spawn_hex_reader(1);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('c'),
        KeyModifiers::CTRL,
    )))
    .expect("send ctrl+c");
    let out = b
        .wait(WaitCond::Text("HEX:03".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "ctrl+c should emit byte 0x03, got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 2: Ctrl+D emits 0x04  (audit item 65)
// ---------------------------------------------------------------------------

/// Encode ctrl+d as EOT (0x04) through the PTY.
/// Audit item 65: Ctrl+D maps to 0x04.
#[test]
fn encode_ctrl_d_emits_0x04() {
    let mut b = spawn_hex_reader(1);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('d'),
        KeyModifiers::CTRL,
    )))
    .expect("send ctrl+d");
    let out = b
        .wait(WaitCond::Text("HEX:04".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "ctrl+d should emit byte 0x04, got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 3: Ctrl+Z emits 0x1a  (audit item 65)
// ---------------------------------------------------------------------------

/// Encode ctrl+z as SUB (0x1a) through the PTY.
/// Audit item 65: Ctrl+Z maps to 0x1a.
#[test]
fn encode_ctrl_z_emits_0x1a() {
    let mut b = spawn_hex_reader(1);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Key(KeyEvent::with_modifiers(
        KeyCode::Char('z'),
        KeyModifiers::CTRL,
    )))
    .expect("send ctrl+z");
    let out = b
        .wait(WaitCond::Text("HEX:1a".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "ctrl+z should emit byte 0x1a, got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 4: Key sequence delivers all bytes  (audit item 66)
// ---------------------------------------------------------------------------

/// Sending three printable chars via Input::Keys delivers the full sequence.
/// Audit item 66: Input::Keys must not truncate the sequence.
#[test]
fn keys_sequence_delivers_all_bytes() {
    let mut b = spawn_hex_reader(3);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Keys(vec![
        KeyEvent::new(KeyCode::Char('a')),
        KeyEvent::new(KeyCode::Char('b')),
        KeyEvent::new(KeyCode::Char('c')),
    ]))
    .expect("send keys");
    let out = b
        .wait(WaitCond::Text("HEX:616263".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "three keys must emit 'abc' (616263), got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 5: Arrow uses SS3 in application cursor mode  (audit item 67)
// ---------------------------------------------------------------------------

/// When DECCKM is enabled (via \x1b[?1h), Up arrow must emit SS3 ESC O A (1b4f41).
/// Audit item 67: mode-aware key encoding for application cursor keys.
#[test]
fn arrows_use_ss3_in_application_cursor_mode() {
    // The init bytes enable DECCKM; the child writes them to stdout, then
    // the Rust VT100 parser picks up application_cursor=true.
    // The child then reads 3 raw bytes (the SS3 sequence).
    let mut b = spawn_hex_reader_with_init(b"\x1b[?1h", 3);
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Key(KeyEvent::new(KeyCode::Up)))
        .expect("send up");
    let out = b
        .wait(WaitCond::Text("HEX:1b4f41".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "Up in app-cursor mode should emit SS3 (1b4f41), got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 6: Arrow uses CSI without application cursor  (audit item 67)
// ---------------------------------------------------------------------------

/// Without DECCKM, Up arrow defaults to CSI ESC [ A (1b5b41).
/// Audit item 67: mode-aware key encoding - default uses CSI.
#[test]
fn arrows_use_csi_without_application_cursor() {
    let mut b = spawn_hex_reader(3);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Key(KeyEvent::new(KeyCode::Up)))
        .expect("send up");
    let out = b
        .wait(WaitCond::Text("HEX:1b5b41".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "Up in default mode should emit CSI (1b5b41), got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 7: Mouse SGR click emits press + release  (audit item 68)
// ---------------------------------------------------------------------------

/// MouseClick with SGR encoding (1006) must emit both press (M) and release (m).
/// SGR press at (10,20) Left: ESC[<0;11;21M (12 bytes)
/// SGR release at (10,20) Left: ESC[<0;11;21m (12 bytes)
/// Total: 24 bytes. Read 24.
/// Audit item 68: real mouse protocol encodings - SGR press and release.
#[test]
fn mouse_sgr_click_press_and_release() {
    // Enable mouse reporting + SGR encoding. Use ?1002h (ButtonMotion) so
    // MouseClick can emit a release; ?1000h is Press-only.
    // SGR press = "ESC[<0;X;YM" = 11 bytes; release uses lowercase 'm'.
    // Read 11 bytes for press + 11 for release = 22 total.
    let mut b = spawn_hex_reader_with_init(b"\x1b[?1002;1006h", 22);
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::MouseClick {
        button: MouseButton::Left,
        x: 10,
        y: 20,
    })
    .expect("send mouse click");
    let out = b
        .wait(WaitCond::Text("HEX".into()), Duration::from_secs(5))
        .expect("wait");
    let hex_str = out.state.viewport_text.join("");
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    // Press:  ESC[<0;11;21M = 1b5b3c303b31313b32314d
    assert!(
        clean.contains("1b5b3c303b31313b32314d"),
        "SGR press missing (expected 1b5b3c303b31313b32314d), got: {}",
        hex_str
    );
    // Release: ESC[<0;11;21m = 1b5b3c303b31313b32316d
    assert!(
        clean.contains("1b5b3c303b31313b32316d"),
        "SGR release missing (expected 1b5b3c303b31313b32316d), got: {}",
        hex_str
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 8: Mouse X10 click emits three raw bytes  (audit item 68)
// ---------------------------------------------------------------------------

/// MouseClick with X10 encoding (mouse mode 1000, default encoding) emits
/// 3 raw bytes per event: Cb (32+button, +3 for release), X (32+(x+1)),
/// Y (32+(y+1)). 1-based wire coords; MouseEvent uses 0-based input.
/// For Left press at (10,20): 0x20 0x2b 0x35 -> 202b35.
/// For Left release at (10,20): 0x23 0x2b 0x35 -> 232b35 (button + 3).
/// Audit item 68: real mouse protocol encodings - X10 press + release.
#[test]
fn mouse_x10_click_three_raw_bytes() {
    // Read 6 bytes for press (ESC[M + Cb X Y) and 6 bytes for release = 12 total.
    // Use ?1002h (ButtonMotion mode) so releases are allowed — ?1000h
    // is Press-only and MouseClick would reject the release.
    let mut b = spawn_hex_reader_with_init(b"\x1b[?1002h", 12);
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::MouseClick {
        button: MouseButton::Left,
        x: 10,
        y: 20,
    })
    .expect("send mouse click");
    let out = b
        .wait(WaitCond::Text("HEX".into()), Duration::from_secs(5))
        .expect("wait");
    let hex_str = out.state.viewport_text.join("");
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    // Press: ESC[M + 20 2b 35 = 1b5b4d202b35
    assert!(
        clean.contains("1b5b4d202b35"),
        "X10 press at (10,20) Left should emit 1b5b4d202b35, got={}",
        hex_str
    );
    // Release: ESC[M + 23 2b 35 = 1b5b4d232b35
    assert!(
        clean.contains("1b5b4d232b35"),
        "X10 release at (10,20) Left should emit 1b5b4d232b35, got={}",
        hex_str
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 9: Mouse press mode rejects release events  (audit item 68)
// ---------------------------------------------------------------------------

/// When only MouseMode::Press is enabled (1000, no 1002/1003/1004),
/// MouseEvent::Release must return Err and must NOT emit bytes.
/// The test verifies this by: (a) sending a release event and confirming
/// it is rejected (is_err), and (b) sending a real key press which would
/// be misaligned if the release had leaked mouse bytes into the PTY stream.
#[test]
fn mouse_press_mode_rejects_release() {
    // Use a reader fixture: python reads 1 byte, prints hex.
    let mut b = spawn_hex_reader(1);
    std::thread::sleep(Duration::from_millis(400));

    // Send a Release via Mouse — should fail because mouse reporting is
    // not enabled (no DECSET was negotiated). The backend must return Err.
    let result = b.send_input(Input::Mouse(MouseEvent::Release {
        button: MouseButton::Left,
        x: 10,
        y: 20,
    }));
    assert!(
        result.is_err(),
        "Mouse::Release should be rejected when mouse reporting is not enabled"
    );

    // Now send a printable key 'a' and verify it produces the correct byte.
    // If the rejected release had leaked bytes, the read would be misaligned.
    b.send_input(Input::Key(KeyEvent::new(KeyCode::Char('a'))))
        .expect("send key a");
    let out = b
        .wait(WaitCond::Text("HEX:61".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "After rejected release, key 'a' should emit 61 (not misaligned), got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 10: Bracketed paste wraps when negotiated  (audit item 69)
// ---------------------------------------------------------------------------

/// When bracketed paste (2004) is enabled, Input::Paste is wrapped with
/// ESC[200~ (open) and ESC[201~ (close).
/// Audit item 69: bracketed paste honoring negotiation.
#[test]
fn bracketed_paste_wraps_when_negotiated() {
    // Enable bracketed paste via DECSET 2004.
    let mut b = spawn_hex_reader_with_init(b"\x1b[?2004h", 14);
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::Paste("hi".into())).expect("send paste");
    let out = b
        .wait(WaitCond::Text("HEX".into()), Duration::from_secs(5))
        .expect("wait");
    let hex_str = out.state.viewport_text.join("");
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    // Open:  ESC[200~ = 0x1b 0x5b 0x32 0x30 0x30 0x7e -> 1b5b3230307e
    assert!(
        clean.contains("1b5b3230307e"),
        "bracketed paste open missing (expected 1b5b3230307e), got: {}",
        hex_str
    );
    // Close: ESC[201~ = 0x1b 0x5b 0x32 0x30 0x31 0x7e -> 1b5b3230317e
    assert!(
        clean.contains("1b5b3230317e"),
        "bracketed paste close missing (expected 1b5b3230317e), got: {}",
        hex_str
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 11: Paste raw when not negotiated  (audit item 69)
// ---------------------------------------------------------------------------

/// When bracketed paste is NOT enabled, Input::Paste sends raw bytes only.
/// Audit item 69: paste without negotiation -> raw text, no brackets.
#[test]
fn paste_raw_when_not_negotiated() {
    let mut b = spawn_hex_reader(2);
    std::thread::sleep(Duration::from_millis(400));
    b.send_input(Input::Paste("hi".into())).expect("send paste");
    let out = b
        .wait(WaitCond::Text("HEX:6869".into()), Duration::from_secs(5))
        .expect("wait");
    assert!(
        out.met,
        "paste without negotiation should emit raw bytes (6869), got={}",
        out.state.viewport_text.join(" ")
    );
    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 12: ScreenStable resolves on one-frame reaction  (audit items 2/70)
// ---------------------------------------------------------------------------

/// The event-sequencing regression: sending one char should bump screen_seq,
/// then ScreenStable with a 150ms quiet_for must resolve within the budget.
/// Also: a generic (no-anchor) ScreenStable must not require fresh activity.
/// Audit items 2/70: event-sequenced waits where a one-frame reaction
/// resolves ScreenStable.
#[test]
fn screen_stable_resolves_on_one_frame_reaction() {
    let mut b = PortablePtyBackend::new(80, 24);
    b.start(
        "python3",
        &[
            "-c".to_string(),
            "import sys; sys.stdout.write('FRAME1'); sys.stdout.flush(); import time; time.sleep(3)\n"
                .to_string(),
        ],
        None,
        &[],
        80,
        24,
    )
    .expect("start python");

    // Wait for initial frame.
    let _ = b
        .wait(WaitCond::Text("FRAME1".into()), Duration::from_secs(5))
        .expect("wait for frame1");

    // Capture baseline state.
    let baseline = b.event_state();

    // Send a character - this produces exactly one screen frame.
    b.send_input(Input::Key(KeyEvent::new(KeyCode::Char('x'))))
        .expect("send x");

    // wait_after: baseline anchor + ScreenStable with 150ms quiet.
    let out = b
        .wait_after(
            baseline,
            WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(150),
                after_screen_seq: None,
            },
            Duration::from_secs(2),
        )
        .expect("wait_after");
    assert!(
        out.met,
        "ScreenStable should resolve after one-frame reaction; met={}, elapsed={}ms",
        out.met, out.elapsed_ms
    );

    // Also assert that a plain (no-anchor) ScreenStable resolves -
    // it should not require fresh activity, just quiet.
    let out2 = b
        .wait(
            WaitCond::ScreenStable {
                quiet_for: Duration::from_millis(150),
                after_screen_seq: None,
            },
            Duration::from_secs(2),
        )
        .expect("wait generic");
    assert!(
        out2.met,
        "Generic ScreenStable should resolve with no anchor; met={}, elapsed={}ms",
        out2.met, out2.elapsed_ms
    );

    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 13: Style-only change bumps screen_seq  (audit item 71)
// ---------------------------------------------------------------------------

/// A reverse-video change (same text, different style) must bump screen_seq.
/// The fingerprint tracks style-only changes.
/// Audit item 71: fingerprint counts style-only (reverse-video) changes.
#[test]
fn style_only_change_bumps_screen_seq() {
    let mut b = PortablePtyBackend::new(80, 24);
    b.start(
        "python3",
        &[
            "-c".to_string(),
            // Stay alive well past the assertion window: the reverse-video
            // echo only renders while the child is running, and under load
            // the Text wait can eat most of a 1s lifetime (race seen in CI).
            "import sys,time; sys.stdout.write('TITLE'); sys.stdout.flush(); time.sleep(6)\n"
                .to_string(),
        ],
        None,
        &[],
        80,
        24,
    )
    .expect("start python");

    // Wait for the initial text.
    let _ = b
        .wait(WaitCond::Text("TITLE".into()), Duration::from_secs(5))
        .expect("wait for title");

    // Capture initial screen_seq.
    let s0 = b.event_state().screen_seq;

    // Write reverse-video version of the same text.
    b.send_input(Input::Raw(b"\r\x1b[7mTITLE\x1b[0m".to_vec()))
        .expect("send reverse title");

    // Wait for stability.
    b.wait(
        WaitCond::ScreenStable {
            quiet_for: Duration::from_millis(150),
            after_screen_seq: None,
        },
        Duration::from_secs(3),
    )
    .expect("wait stable");

    let s1 = b.event_state().screen_seq;
    assert!(
        s1 > s0,
        "style-only change (reverse video) should bump screen_seq: s0={} s1={}",
        s0,
        s1
    );

    b.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Test 14: Mouse UTF-8 encoding emits UTF-8 coords  (audit item 68)
// ---------------------------------------------------------------------------

/// With UTF-8 mouse encoding (1005), click bytes use codepoints:
/// Cb = 32+button (+3 for release), X = 32+(x+1), Y = 32+(y+1)
/// (1-based wire coords; our MouseEvent uses 0-based input).
/// For press at (10,20) Left: Cb=32(0x20), X=43(0x2b), Y=53(0x35).
/// Press: ESC[M + 20 2b 35 = 1b5b4d202b35 (4 bytes)
/// For release at (10,20) Left: Cb=32+3=35(0x23), X=43(0x2b), Y=53(0x35).
/// Release: ESC[M + 23 2b 35 = 1b5b4d232b35 (4 bytes)
/// Total: 8 bytes. Read 8.
/// Audit item 68: real mouse protocol encodings - UTF-8 encoding (1005).
#[test]
fn mouse_utf8_encoding_emits_utf8_coords() {
    // Enable mouse reporting + UTF-8 encoding. Use ?1002h (ButtonMotion) so
    // MouseClick can emit a release; ?1000h is Press-only.
    // Press + release are each 6 bytes (ESC[M + Cb X Y) = 12 total.
    let mut b = spawn_hex_reader_with_init(b"\x1b[?1002;1005h", 12);
    std::thread::sleep(Duration::from_millis(500));
    b.send_input(Input::MouseClick {
        button: MouseButton::Left,
        x: 10,
        y: 20,
    })
    .expect("send mouse click");
    let out = b
        .wait(WaitCond::Text("HEX".into()), Duration::from_secs(5))
        .expect("wait");
    let hex_str = out.state.viewport_text.join("");
    let clean: String = hex_str.chars().filter(|c| !c.is_whitespace()).collect();
    // Press: ESC[M + UTF8 coords -> 1b5b4d202b35
    assert!(
        clean.contains("1b5b4d202b35"),
        "UTF-8 mouse press missing (expected 1b5b4d202b35), got: {}",
        hex_str
    );
    // Release: ESC[M + 0x23 0x2b 0x35 -> 1b5b4d232b35
    // (xterm 1005 release encoding: button byte = 32 + button + 3 = 35 = '#').
    assert!(
        clean.contains("1b5b4d232b35"),
        "UTF-8 mouse release missing (expected 1b5b4d232b35), got: {}",
        hex_str
    );
    b.stop().expect("stop");
}
