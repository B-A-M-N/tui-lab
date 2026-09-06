//! Pure terminal-input encoders (god-object round 2, G2).
//!
//! `encode_key` / `encode_key_kitty` / `encode_mouse_event` move OUT of
//! `portable_pty.rs` verbatim: they are pure protocol encoders with no
//! dependency on the PTY process, the parser, or any backend state — they
//! take typed input and negotiated modes and return bytes. The backend
//! calls them from `send_input`; the byte-level conformance fixtures moved
//! with them (mouse SGR/X10/UTF-8; modified navigation keys).
//!
//! No behavior change: this is a physical move, byte-for-byte identical
//! encodings.

use crate::backend::{
    BackendError, BackendResult, InputModes, KeyEvent, KeyModifiers, MouseEncoding, MouseEvent,
    ScrollDirection,
};

/// Encode a typed [`KeyEvent`] into raw terminal bytes (xterm-style).
///
/// Mode-aware: arrow keys and Home/End switch between SS3 and CSI based on
/// application cursor mode; Shift+ASCII letter emits the uppercase byte;
/// SUPER was historically rejected (unsupported without kitty keyboard
/// protocol) (audit item 6). Wave F item 52: when the application pushed
/// kitty keyboard flags (`CSI > flags u`), the CSI-u encoding unlocks —
/// Super-modified keys and F-keys above F12 encode faithfully instead of
/// being rejected.
pub(crate) fn encode_key(kev: &KeyEvent, modes: &InputModes) -> BackendResult<Vec<u8>> {
    use crate::backend::KeyCode::*;
    let KeyEvent { code, modifiers } = kev;

    // Kitty CSI-u encoding (item 52): active when the application pushed
    // flags. Covers every key we model; disambiguates Super and high
    // function keys that legacy encodings cannot express. The key code
    // follows the kitty spec's unicode-key-code table (Enter=13, Tab=9,
    // Escape=27, Backspace=127, arrows=1(A)…; F1–F12 = 57364–57375 in
    // functional-key space, but legacy numbers 11–24 are accepted too).
    if modes.kitty_flags > 0 {
        if let Some(bytes) = encode_key_kitty(kev) {
            return Ok(bytes);
        }
        // Fall through to legacy encodings when the key has no kitty form.
    }

    // Reject SUPER — cannot encode without the kitty keyboard protocol.
    if modifiers.contains(KeyModifiers::SUPER) {
        return Err(BackendError::Unsupported(
            "super/meta is not encodable without the kitty keyboard protocol".into(),
        ));
    }

    // C0 control from Ctrl+letter/control char.
    if modifiers.ctrl() {
        match code {
            Char(c) if c.is_ascii_lowercase() => {
                return Ok(vec![*c as u8 - b'a' + 1]);
            }
            Char(c) if c.is_ascii_uppercase() => {
                // Ctrl+Shift+Letter: send the uppercase control byte.
                return Ok(vec![*c as u8 - b'A' + 1]);
            }
            Char(' ') => return Ok(vec![0x00]), // Ctrl+Space
            Char('@') => return Ok(vec![0x00]),
            Char('2') => return Ok(vec![0x00]), // Ctrl+@
            Char('[') | Char('{') => return Ok(vec![0x1b]), // Ctrl+[
            Char(']') | Char('}') => return Ok(vec![0x1d]),
            Char('\\') => return Ok(vec![0x1c]),
            Char('^') => return Ok(vec![0x1e]),
            Char('_') => return Ok(vec![0x1f]),
            _ => {}
        }
    }

    let app_cursor = modes.application_cursor;
    let shift = modifiers.shift();
    let alt = modifiers.alt();

    // Modified navigation keys: xterm modifyOtherKeys-style CSI sequences
    // (re-review Wave-1 item 7 — previously Ctrl+Arrow silently degraded to
    // a bare arrow, so the application could not tell it received a modified
    // key). Format: CSI 1;<mods>{A,B,C,D} for arrows, CSI 1;<mods>{H,F} for
    // Home/End, CSI <n>;<mods>~ for PgUp/PgDn/Insert/Delete. Modifier mask is
    // 1 + shift(1) + alt(2) + ctrl(4). Unmodified keys fall through to the
    // plain encodings below (including SS3 application-cursor variants).
    let xterm_mods = 1 + (shift as u8) + ((alt as u8) << 1) + ((modifiers.ctrl() as u8) << 2);
    if shift || alt || modifiers.ctrl() {
        match code {
            Up | Down | Left | Right => {
                let ch = match code {
                    Up => 'A',
                    Down => 'B',
                    Right => 'C',
                    _ => 'D',
                };
                return Ok(format!("\x1b[1;{}{}", xterm_mods, ch).into_bytes());
            }
            Home | End => {
                let ch = if matches!(code, Home) { 'H' } else { 'F' };
                return Ok(format!("\x1b[1;{}{}", xterm_mods, ch).into_bytes());
            }
            PageUp | PageDown | Insert | Delete => {
                let n = match code {
                    PageUp => 5,
                    PageDown => 6,
                    Insert => 2,
                    _ => 3,
                };
                return Ok(format!("\x1b[{};{}~", n, xterm_mods).into_bytes());
            }
            _ => {}
        }
    }

    let base: Vec<u8> = match code {
        Char(c) if *c == ' ' => b" ".to_vec(),
        Char(c) => {
            // Shift+ASCII letter → uppercase byte (audit item 6).
            if shift && c.is_ascii_alphabetic() {
                vec![c.to_ascii_uppercase() as u8]
            } else if shift {
                // Non-letter shift: send the character as-is.
                let mut s = String::new();
                s.push(*c);
                s.into_bytes()
            } else {
                let mut s = String::new();
                s.push(*c);
                s.into_bytes()
            }
        }
        Enter => b"\r".to_vec(),
        Tab => {
            if shift {
                b"\x1b[Z".to_vec() // Shift+Tab
            } else {
                b"\t".to_vec()
            }
        }
        Backspace => b"\x7f".to_vec(),
        Escape => b"\x1b".to_vec(),
        // Arrow keys: SS3 in application cursor mode, CSI otherwise (audit item 6).
        Up if app_cursor => b"\x1bOA".to_vec(),
        Down if app_cursor => b"\x1bOB".to_vec(),
        Right if app_cursor => b"\x1bOC".to_vec(),
        Left if app_cursor => b"\x1bOD".to_vec(),
        Up => b"\x1b[A".to_vec(),
        Down => b"\x1b[B".to_vec(),
        Right => b"\x1b[C".to_vec(),
        Left => b"\x1b[D".to_vec(),
        // Home/End: SS3 variants in application cursor mode (audit item 6).
        Home if app_cursor => b"\x1bOH".to_vec(),
        End if app_cursor => b"\x1bOF".to_vec(),
        Home => b"\x1b[H".to_vec(),
        End => b"\x1b[F".to_vec(),
        PageUp => b"\x1b[5~".to_vec(),
        PageDown => b"\x1b[6~".to_vec(),
        Insert => b"\x1b[2~".to_vec(),
        Delete => b"\x1b[3~".to_vec(),
        Function(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => {
                // Unsupportable without kitty keyboard protocol or a
                // custom escape sequence; return Err so callers learn the
                // action was rejected instead of silently emitting zero
                // bytes (re-review P0).
                return Err(BackendError::Unsupported(format!(
                    "function key F{} is not encodable without the kitty keyboard protocol",
                    n
                )));
            }
        },
    };

    if alt {
        // ESC-prefix for Alt+key (xterm default).
        let mut v = b"\x1b".to_vec();
        v.extend_from_slice(&base);
        Ok(v)
    } else {
        Ok(base)
    }
}

/// Wave F item 52: kitty keyboard protocol (CSI-u) encoding.
///
/// `CSI unicode-key-code ; modifiers [event-type] u`. Modifiers are
/// 1 + shift(1) + alt(2) + ctrl(4) + super(8). Returns `None` for keys with
/// no kitty unicode-key-code (the caller falls through to legacy encodings).
pub(crate) fn encode_key_kitty(kev: &KeyEvent) -> Option<Vec<u8>> {
    use crate::backend::KeyCode::*;
    let KeyEvent { code, modifiers } = kev;
    let key_code: u32 = match code {
        Char(c) => *c as u32,
        Escape => 27,
        Enter => 13,
        Tab => 9,
        Backspace => 127,
        Up => 0xE000, // kitty functional: 57344 + n
        Down => 0xE000 + 1,
        Left => 0xE000 + 2,
        Right => 0xE000 + 3,
        Home => 0xE000 + 4,
        End => 0xE000 + 5,
        Insert => 0xE000 + 6,
        Delete => 0xE000 + 7,
        PageUp => 0xE000 + 8,
        PageDown => 0xE000 + 9,
        Function(n) => match n {
            // F1–F12 map to the kitty functional-key block.
            1..=12 => 0xE000 + 12 + (*n as u32 - 1),
            // F13–F20 continue the block.
            13..=20 => 0xE000 + 12 + (*n as u32 - 1),
            _ => return None,
        },
    };
    let mods = 1u8
        + (modifiers.contains(KeyModifiers::SHIFT) as u8)
        + ((modifiers.contains(KeyModifiers::ALT) as u8) << 1)
        + ((modifiers.contains(KeyModifiers::CTRL) as u8) << 2)
        + ((modifiers.contains(KeyModifiers::SUPER) as u8) << 3);
    if mods == 1 {
        Some(format!("\x1b[{}u", key_code).into_bytes())
    } else {
        Some(format!("\x1b[{};{}u", key_code, mods).into_bytes())
    }
}

/// Encode a typed [`MouseEvent`] into the negotiated protocol bytes.
///
/// Dispatches SGR (1006), Default/X10 (1000), and UTF-8 (1005) encodings.
///
/// Byte-level semantics (re-review P0 mouse conformance):
///
/// * Coordinates are 1-based on the wire for **all** encodings (classic
///   xterm convention). Our `MouseEvent` uses 0-based coordinates, so we
///   add `+ 1` before the `+ 32` encoding offset.
/// * Wheel events use button code 64 (up) / 65 (down) per xterm.
/// * Motion-while-button-held (drag) sets bit 5 (0x20) of the button code.
/// * X10/UTF-8 release sets the high bit of the button byte via the
///   `button + 3 + 32` convention used by xterm; this is the legacy way
///   to distinguish release from press when the encoding has no other
///   mechanism (vt100 decoders expect this). SGR release uses the
///   trailing lowercase byte `m` instead of `M`.
/// * Coordinates > 222 cannot be expressed in X10/UTF-8 classic mode and
///   return `Err(Unsupported)`; SGR has no such limit.
pub(crate) fn encode_mouse_event(
    ev: &MouseEvent,
    encoding: MouseEncoding,
) -> BackendResult<Vec<u8>> {
    // Compute Cb button base, coords, and flags. Release in X10/UTF-8
    // uses `button_base + 3` (the legacy xterm release offset); SGR
    // release uses the lowercase trailer byte below and so shares the
    // press base.
    let (button_base, x, y, release_offset, motion) = match ev {
        MouseEvent::Press { button, x, y } => (button.sgr_base(), *x, *y, 0u8, false),
        MouseEvent::Release { button, x, y } => (button.sgr_base(), *x, *y, 3u8, false),
        // Hover motion (no button held): xterm uses button code 3 plus the
        // motion bit (byte 67/'C' in X10, Cb 35 in SGR). Without the motion
        // bit the byte collides with the left-button release encoding.
        MouseEvent::Move { x, y } => (3, *x, *y, 0u8, true),
        MouseEvent::Drag { button, x, y } => (button.sgr_base(), *x, *y, 0u8, true),
        MouseEvent::Scroll { direction, x, y } => {
            let base = match direction {
                ScrollDirection::Up => 64,
                ScrollDirection::Down => 65,
            };
            (base, *x, *y, 0u8, false)
        }
    };

    // X10/UTF-8 clamp coordinates > 222 (255 - 32). The on-wire byte is
    // `(value_1_based) + 32`, so the maximum representable coordinate is
    // 222. Apply this check up-front so all three branches share it.
    let x10_clamp = |label: &'static str| -> BackendResult<()> {
        if x > 222 || y > 222 {
            return Err(BackendError::Unsupported(format!(
                "{label} mouse encoding cannot express coordinates > 222 (got {x},{y})"
            )));
        }
        Ok(())
    };

    match encoding {
        MouseEncoding::Sgr => {
            // SGR 1006: ESC [ < Cb ; X ; Y M/m  (1-based coords).
            // The wheel codes (64/65) already live in the low 6 bits, so no
            // mask is applied: `& 0x3f` would collapse wheel-up (64) into
            // button 0 and every scroll would decode as a left click.
            let mut b = button_base;
            if motion {
                b |= 0x20;
            }
            let trailer = if matches!(ev, MouseEvent::Release { .. }) {
                'm'
            } else {
                'M'
            };
            Ok(format!("\x1b[<{};{};{}{}", b, x + 1, y + 1, trailer).into_bytes())
        }
        MouseEncoding::Default => {
            // X10 1000: ESC [ M Cb' X' Y'  where each byte = (1_based) + 32.
            // Drag sets the motion bit (0x20) of the button byte.
            // Release uses the legacy xterm +3 offset on the button byte.
            x10_clamp("x10")?;
            let cb = button_base + release_offset + 32 + if motion { 0x20 } else { 0 };
            // Wire format is 1-based; bump internal 0-based coords by 1.
            let cx = (x + 1 + 32) as u8;
            let cy = (y + 1 + 32) as u8;
            Ok(vec![0x1b, b'[', b'M', cb, cx, cy])
        }
        MouseEncoding::Utf8 => {
            // UTF-8 xterm extension (1005): ESC [ M then each of Cb, X, Y
            // encoded as a UTF-8 codepoint at (1_based + 32). Drag sets the
            // motion bit on the button byte; release uses +3.
            x10_clamp("utf8")?;
            let cb = button_base + release_offset + 32 + if motion { 0x20 } else { 0 };
            let mut out = vec![0x1b, b'[', b'M'];
            out.push(cb);
            out.push((x + 1 + 32) as u8);
            out.push((y + 1 + 32) as u8);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod mouse_encode_tests {
    //! Byte-level mouse conformance fixtures (re-review P0 "required backend
    //! conformance tests"). Expectations follow xterm ctlseqs: coordinates are
    //! 1-based on the wire for all encodings, X10/UTF-8 bytes are
    //! (1-based value + 32), wheel buttons are 64/65, the motion bit is 0x20,
    //! and X10/UTF-8 release adds 3 to the button code.
    use super::*;
    use crate::backend::{MouseButton, MouseEncoding, MouseEvent, ScrollDirection};

    fn enc(ev: MouseEvent, encoding: MouseEncoding) -> Vec<u8> {
        encode_mouse_event(&ev, encoding).expect("encode")
    }

    fn ev_bytes(ev: MouseEvent, encoding: MouseEncoding) -> String {
        enc(ev, encoding)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    // ── SGR (1006) ──────────────────────────────────────────────────────

    #[test]
    fn sgr_press_left_at_10_20() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c303b31313b32314d"
        ); // ESC[<0;11;21M
    }

    #[test]
    fn sgr_release_uses_lowercase_m() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c303b31313b32316d"
        ); // ESC[<0;11;21m
    }

    #[test]
    fn sgr_right_button_press_is_2() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Right,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c323b313b314d"
        ); // ESC[<2;1;1M
    }

    #[test]
    fn sgr_move_has_motion_bit_and_button_3() {
        // Hover motion: Cb 3 | 0x20 = 35, trailer M.
        assert_eq!(
            ev_bytes(MouseEvent::Move { x: 5, y: 6 }, MouseEncoding::Sgr),
            "1b5b3c33353b363b374d"
        ); // ESC[<35;6;7M
    }

    #[test]
    fn sgr_drag_sets_motion_bit_on_button() {
        // Left drag: Cb 0 | 0x20 = 32.
        assert_eq!(
            ev_bytes(
                MouseEvent::Drag {
                    button: MouseButton::Left,
                    x: 3,
                    y: 4
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c33323b343b354d"
        ); // ESC[<32;4;5M
    }

    #[test]
    fn sgr_wheel_up_is_64_not_button_zero() {
        // Regression: the removed `& 0x3f` mask collapsed 64 into button 0,
        // encoding every scroll as a left click.
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Up,
                    x: 1,
                    y: 1
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c36343b323b324d"
        ); // ESC[<64;2;2M
    }

    #[test]
    fn sgr_wheel_down_is_65() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Down,
                    x: 1,
                    y: 1
                },
                MouseEncoding::Sgr
            ),
            "1b5b3c36353b323b324d"
        ); // ESC[<65;2;2M
    }

    #[test]
    fn sgr_coordinates_beyond_222_are_allowed() {
        // SGR carries decimal coordinates; no X10 clamp applies.
        let out = enc(
            MouseEvent::Press {
                button: MouseButton::Left,
                x: 500,
                y: 300,
            },
            MouseEncoding::Sgr,
        );
        let s = String::from_utf8(out).expect("utf8");
        assert_eq!(s, "\x1b[<0;501;301M");
    }

    // ── X10 / Default (1000) ────────────────────────────────────────────

    #[test]
    fn x10_press_left_at_10_20() {
        // Cb=0+32=0x20, X=11+32=0x2b, Y=21+32=0x35 (1-based wire coords).
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Default
            ),
            "1b5b4d202b35"
        );
    }

    #[test]
    fn x10_release_adds_3_to_button_code() {
        // Left release: Cb=0+3+32=0x23.
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Default
            ),
            "1b5b4d232b35"
        );
    }

    #[test]
    fn x10_middle_and_right_releases() {
        // Middle (1): Cb=1+3+32=0x24. Right (2): Cb=2+3+32=0x25.
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Middle,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d242121"
        );
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Right,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d252121"
        );
    }

    #[test]
    fn x10_drag_sets_motion_bit() {
        // Left drag: Cb=0|0x20 then +32 = 0x40; X=8+32=0x28, Y=9+32=0x29.
        assert_eq!(
            ev_bytes(
                MouseEvent::Drag {
                    button: MouseButton::Left,
                    x: 7,
                    y: 8
                },
                MouseEncoding::Default
            ),
            "1b5b4d402829"
        );
    }

    #[test]
    fn x10_move_is_button_3_plus_motion_bit() {
        // Hover: Cb=3|0x20=35, byte=35+32=67=0x43 ('C'). Distinct from any
        // release byte.
        assert_eq!(
            ev_bytes(MouseEvent::Move { x: 0, y: 0 }, MouseEncoding::Default),
            "1b5b4d432121"
        );
    }

    #[test]
    fn x10_wheel_up_and_down() {
        // Up: Cb=64+32=0x60. Down: Cb=65+32=0x61.
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Up,
                    x: 2,
                    y: 3
                },
                MouseEncoding::Default
            ),
            "1b5b4d602324"
        );
        assert_eq!(
            ev_bytes(
                MouseEvent::Scroll {
                    direction: ScrollDirection::Down,
                    x: 2,
                    y: 3
                },
                MouseEncoding::Default
            ),
            "1b5b4d612324"
        );
    }

    #[test]
    fn x10_rejects_coordinates_above_222() {
        let out = encode_mouse_event(
            &MouseEvent::Press {
                button: MouseButton::Left,
                x: 300,
                y: 0,
            },
            MouseEncoding::Default,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }

    #[test]
    fn x10_origin_is_1_1_not_0_0() {
        // Internal (0,0) maps to wire bytes 0x21 0x21 ('!' '!') — the 1-based
        // home position — never 0x20 0x20.
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 0,
                    y: 0
                },
                MouseEncoding::Default
            ),
            "1b5b4d202121"
        );
    }

    // ── UTF-8 (1005) ────────────────────────────────────────────────────

    #[test]
    fn utf8_press_matches_x10_in_classic_range() {
        // Within the classic range, 1005 shares X10's byte shape (documented
        // limitation: this backend does not emit multi-byte codepoints for
        // coordinates > 222 — it rejects them instead).
        assert_eq!(
            ev_bytes(
                MouseEvent::Press {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Utf8
            ),
            "1b5b4d202b35"
        );
    }

    #[test]
    fn utf8_release_adds_3() {
        assert_eq!(
            ev_bytes(
                MouseEvent::Release {
                    button: MouseButton::Left,
                    x: 10,
                    y: 20
                },
                MouseEncoding::Utf8
            ),
            "1b5b4d232b35"
        );
    }

    #[test]
    fn utf8_rejects_coordinates_above_222() {
        let out = encode_mouse_event(
            &MouseEvent::Press {
                button: MouseButton::Left,
                x: 223,
                y: 0,
            },
            MouseEncoding::Utf8,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }
}

/// Keyboard conformance fixtures for modified navigation keys (re-review
/// Wave-1 item 7). Without these, Ctrl+Arrow silently degraded to a bare
/// arrow byte sequence.
#[cfg(test)]
mod key_encode_tests {
    use super::*;
    use crate::backend::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        let modes = crate::backend::InputModes::default();
        encode_key(&KeyEvent { code, modifiers }, &modes).expect("encode key")
    }

    fn bytes(v: Vec<u8>) -> String {
        v.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn ctrl_up_is_csi_u_not_bare_arrow() {
        // CSI 1;5A — modifier mask 1+ctrl(4)=5.
        assert_eq!(bytes(key(KeyCode::Up, KeyModifiers::CTRL)), "1b5b313b3541");
    }

    #[test]
    fn shift_right_is_csi_with_mask_2() {
        // CSI 1;2C — 1+shift(1)=2.
        assert_eq!(
            bytes(key(KeyCode::Right, KeyModifiers::SHIFT)),
            "1b5b313b3243"
        );
    }

    #[test]
    fn ctrl_shift_left_mask_is_6() {
        // 1+shift(1)+ctrl(4)=6.
        assert_eq!(
            bytes(key(KeyCode::Left, KeyModifiers::CTRL | KeyModifiers::SHIFT)),
            "1b5b313b3644"
        );
    }

    #[test]
    fn alt_up_uses_csi_with_mask_3() {
        // CSI 1;3A — 1+alt(2)=3 (modifyOtherKeys form; the old ESC-prefixed
        // bare arrow lost the fact that Alt was held).
        let modes = crate::backend::InputModes::default();
        let out = encode_key(
            &KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
            },
            &modes,
        )
        .expect("encode");
        assert_eq!(out, b"\x1b[1;3A".to_vec());
    }

    #[test]
    fn ctrl_home_and_end_use_hf_with_mask() {
        assert_eq!(
            bytes(key(KeyCode::Home, KeyModifiers::CTRL)),
            "1b5b313b3548"
        ); // CSI 1;5H
        assert_eq!(bytes(key(KeyCode::End, KeyModifiers::CTRL)), "1b5b313b3546");
        // CSI 1;5F
    }

    #[test]
    fn modified_pageup_pagedown_insert_delete() {
        // CSI 5;5~ / 6;5~ / 2;5~ / 3;5~.
        assert_eq!(
            bytes(key(KeyCode::PageUp, KeyModifiers::CTRL)),
            "1b5b353b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::PageDown, KeyModifiers::CTRL)),
            "1b5b363b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::Insert, KeyModifiers::CTRL)),
            "1b5b323b357e"
        );
        assert_eq!(
            bytes(key(KeyCode::Delete, KeyModifiers::CTRL)),
            "1b5b333b357e"
        );
    }

    #[test]
    fn unmodified_arrows_still_use_plain_csi() {
        assert_eq!(bytes(key(KeyCode::Up, KeyModifiers::empty())), "1b5b41"); // ESC[A
    }

    #[test]
    fn shift_tab_still_is_csi_z() {
        assert_eq!(bytes(key(KeyCode::Tab, KeyModifiers::SHIFT)), "1b5b5a"); // ESC[Z
    }

    #[test]
    fn ctrl_letter_is_c0_control() {
        assert_eq!(bytes(key(KeyCode::Char('c'), KeyModifiers::CTRL)), "03");
    }

    #[test]
    fn super_still_rejected() {
        let modes = crate::backend::InputModes::default();
        let out = encode_key(
            &KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::SUPER,
            },
            &modes,
        );
        assert!(matches!(out, Err(BackendError::Unsupported(_))));
    }
}
