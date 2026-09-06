//! Terminal protocol observation + device-query responder state
//! (god-object round 2, G2).
//!
//! [`BackendCallbacks`] — the vt100 parser callbacks that record
//! host-observable terminal metadata (title, bells, OSC8 hyperlinks,
//! kitty keyboard flags, OSC 133 shell-integration edges) and compose
//! the device-query answers (DA1/DA2/DA3, DSR, DECRQM, kitty `?u`, OSC
//! color) — moves OUT of `portable_pty.rs` verbatim.
//!
//! The query class becomes a typed enum ([`QueryClass`]) internally;
//! the plan's note about the old `Option<&'static str>` stringly typing.
//! Serialization to the stable event-format strings (`"da1"`,
//! `"dsr_cpr"`, …) happens at the API boundary via
//! [`QueryClass::as_str`] — the persisted `QueryAnswered { class }`
//! format is unchanged (invariant 13).

/// vt100 callbacks that record host-observable terminal metadata.
///
/// Security boundary (spec section 75): these callbacks are *observations
/// only*. We deliberately do NOT act on the host in response to terminal
/// output — e.g. we never write the OSC 52 clipboard to the host clipboard.
#[derive(Default)]
pub(super) struct BackendCallbacks {
    pub(super) title: Option<String>,
    pub(super) audible_bells: u64,
    pub(super) title_seq: u64,
    /// OSC8 hyperlink currently open (`id=…` params + URI), if any.
    pub(super) open_link: Option<crate::screen::cell::Hyperlink>,
    /// Links completed so far, in completion order.
    pub(super) links: Vec<crate::screen::cell::Hyperlink>,
    /// Wave F item 52: kitty keyboard protocol flags pushed by the app
    /// (`CSI > flags u` sets, `CSI < u` pops, `CSI = flags ; mode u` sets
    /// the active portion). We track the *current* stack top honestly —
    /// a full stack is only needed if apps interleave, which none do.
    pub(super) kitty_flags: u8,
    /// Whether the app ever pushed kitty flags (capability promotion).
    pub(super) kitty_seen: bool,
    /// Wave F item 56: bytes the terminal should send back to the
    /// application in response to queries (DA1/DA2, DSR cursor position,
    /// DECRQM mode reports, kitty `?u`, OSC color queries). Drained back
    /// to the PTY by `pump()`.
    pub(super) query_responses: Vec<u8>,
    /// Wave F item 54: shell-integration command edges (OSC 133).
    pub(super) command_seq: u64,
    pub(super) command_running: bool,
    pub(super) last_command_exit: Option<i32>,
    pub(super) command_phase: &'static str,
    /// Item 22: query/answer bookkeeping. When the responder queues an
    /// answer, it records the class here; `pump()` promotes the pending
    /// class to `last_query` at the moment it actually writes the answer
    /// bytes back to the PTY — that write is the measured "answer sent at".
    /// Monotonic counters (one per answered query) let the session layer
    /// diff "answers since last observe" into terminal events.
    pub(super) answered_seq: u64,
    pub(super) pending_class: Option<QueryClass>,
}

impl BackendCallbacks {
    fn queue_response(&mut self, bytes: &[u8]) {
        self.query_responses.extend_from_slice(bytes);
    }

    /// Item 22: name the query class this answer belongs to and bump the
    /// answer counter. Called by every responder arm right before (or right
    /// after) queueing the reply bytes.
    fn note_answer(&mut self, class: QueryClass) {
        self.answered_seq += 1;
        self.pending_class = Some(class);
    }
}

impl vt100::Callbacks for BackendCallbacks {
    fn audible_bell(&mut self, _screen: &mut vt100::Screen) {
        self.audible_bells += 1;
    }
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        // Observation only: record the requested title. Never act on the host.
        let s = String::from_utf8_lossy(title).into_owned();
        self.title = Some(s);
        self.title_seq += 1;
    }

    /// OSC8 hyperlinks (Wave C item 29): `\e]8;params;uri\e\\ … \e]8;;\e\\`.
    /// The vt grid does not carry link state, so we record the span by
    /// *cursor position at open/close time* — the same coordinates the grid
    /// uses. Observation only: we never fetch the URI.
    ///
    /// Wave F item 54: OSC 133 shell-integration marks (A=prompt start,
    /// B=command start of input echo, C=command output start, D[;exit]=
    /// command end) are parsed from the same hook. Only edges are recorded —
    /// never command payload text.
    ///
    /// Wave F item 56: OSC 10/11/… `?` color queries get an honest report
    /// (we do not track palette state; we answer with the default colors we
    /// actually render with rather than guessing the app's theme).
    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        let (y, x) = screen.cursor_position();
        match params.first().copied() {
            Some(b"8") => {
                // OSC8 with empty URI closes the current link.
                let uri_at_2: Option<&[u8]> = params.get(2).map(|p| p.as_ref());
                match uri_at_2 {
                    Some(uri) if !uri.is_empty() => {
                        let link_params =
                            String::from_utf8_lossy(params.get(1).copied().unwrap_or(b""));
                        let id = link_params
                            .split(';')
                            .find_map(|kv| kv.strip_prefix("id="))
                            .map(|s| s.to_string());
                        self.open_link = Some(crate::screen::cell::Hyperlink {
                            id,
                            uri: String::from_utf8_lossy(uri).into_owned(),
                            start: (x, y),
                            end: None,
                        });
                    }
                    _ => {
                        // Close: finish the span at the current cursor.
                        if let Some(mut link) = self.open_link.take() {
                            link.end = Some((x, y));
                            self.links.push(link);
                        }
                    }
                }
            }
            // ── OSC 133 shell integration (item 54) ──
            Some(b"133") => match params.get(1).copied() {
                Some(b"A") => {
                    self.command_phase = "prompt";
                }
                Some(b"B") => {
                    self.command_phase = "command";
                }
                Some(b"C") => {
                    self.command_seq += 1;
                    self.command_running = true;
                    self.command_phase = "output";
                }
                Some(b"D") => {
                    self.command_running = false;
                    self.command_phase = "done";
                    if let Some(exit) = params.get(2) {
                        let s = String::from_utf8_lossy(exit);
                        self.last_command_exit = s.trim().parse::<i32>().ok();
                    }
                }
                _ => {}
            },
            // ── Terminal color queries (item 56) ──
            // `OSC 10 ; ? BEL` (foreground), `OSC 11 ; ? BEL` (background),
            // `OSC 4 ; idx ; ? BEL` (palette). We answer with the colors we
            // actually render with — the harness displays default-color cells
            // on a plain terminal, so that is the honest report.
            Some(b"10") | Some(b"11") if params.get(2).copied() == Some(b"?".as_slice()) => {
                let fg = matches!(params.first().copied(), Some(b"10"));
                // xterm dynamic-color report: OSC <n> ; rgb:RRRR/GGGG/BBBB
                let (r, g, b) = if fg {
                    (0xC7u16, 0xC7, 0xC7)
                } else {
                    (0x00, 0x00, 0x00)
                };
                let which = if fg { 10 } else { 11 };
                self.note_answer(QueryClass::OscColor);
                self.queue_response(
                    format!("\x1b]{};rgb:{:04x}/{:04x}/{:04x}\x07", which, r, g, b).as_bytes(),
                );
            }
            _ => {}
        }
    }

    /// Wave F items 52 + 56: CSI sequences vt100 does not implement carry
    /// the kitty keyboard protocol stack ops and the terminal queries.
    fn unhandled_csi(
        &mut self,
        _screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // ── Kitty keyboard protocol (item 52) ──
        // `CSI > flags u`    push flags
        // `CSI < number u`   pop `number` entries (default 1)
        // `CSI = flags ; m u` set active flags (mode 1) / selected (mode 2)
        // `CSI ? u`          query → `CSI ? flags u` response
        if c == 'u' {
            if i1 == Some(b'>') {
                let flags = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                self.kitty_flags = flags.min(u8::MAX as u16) as u8;
                self.kitty_seen = true;
                return;
            }
            if i1 == Some(b'<') {
                let n = params
                    .first()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(1)
                    .max(1);
                // A pop past the bottom of the stack disables the protocol
                // (spec: the stack starts at depth 0 with flags 0).
                self.kitty_flags = 0;
                let _ = n; // single-depth stack: any pop clears
                return;
            }
            if i1 == Some(b'=') {
                let flags = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                match mode {
                    1 => self.kitty_flags = flags.min(u8::MAX as u16) as u8,
                    2 => self.kitty_flags |= flags.min(u8::MAX as u16) as u8,
                    3 => self.kitty_flags &= !(flags.min(u8::MAX as u16) as u8),
                    _ => {}
                }
                self.kitty_seen = true;
                return;
            }
            if i1 == Some(b'?') {
                // Query: report the currently active flags.
                self.note_answer(QueryClass::KittyFlags);
                self.queue_response(format!("\x1b[?{}u", self.kitty_flags).as_bytes());
                return;
            }
        }

        // ── Device queries (item 56) ──
        match (i1, c) {
            // DA1: `CSI c` or `CSI 0 c` → VT100 with AVO (`?1;2c`).
            (None, 'c') | (Some(b'0'), 'c') => {
                self.note_answer(QueryClass::Da1);
                self.queue_response(b"\x1b[?1;2c");
            }
            // Secondary DA: `CSI > c` → vt220, version 1, no ROM.
            (Some(b'>'), 'c') => {
                self.note_answer(QueryClass::Da2);
                self.queue_response(b"\x1b[>0;1;0c");
            }
            // Tertiary DA: `CSI = c` → unit id 0.
            (Some(b'='), 'c') => {
                self.note_answer(QueryClass::Da3);
                self.queue_response(b"\x1bP!|0000\x1b\\");
            }
            // DSR — cursor position: `CSI 6n` → `CSI row ; col R` (1-based).
            (None, 'n') if params.first().and_then(|p| p.first()).copied() == Some(6) => {
                let (row, col) = _screen.cursor_position();
                self.note_answer(QueryClass::DsrCursorPosition);
                self.queue_response(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // DSR — operating status: `CSI 5n` → OK.
            (None, 'n') if params.first().and_then(|p| p.first()).copied() == Some(5) => {
                self.note_answer(QueryClass::DsrStatus);
                self.queue_response(b"\x1b[0n");
            }
            // DECRQM: `CSI ? Ps $ p` → DECSET report; `CSI Ps $ p` → ANSI report.
            (Some(b'?'), 'p') => {
                let mode = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                let set = match mode {
                    1 => _screen.application_cursor(),
                    25 => !_screen.hide_cursor(),
                    1000 | 1002 | 1003 => {
                        _screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
                    }
                    1006 => _screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr,
                    2004 => _screen.bracketed_paste(),
                    1049 => _screen.alternate_screen(),
                    _ => false,
                };
                self.note_answer(QueryClass::Decrqm);
                self.queue_response(format!("\x1b[?{};{}$y", mode, set as u8).as_bytes());
            }
            _ => {}
        }
    }
}

/// The device-query classes the responder answers (item 22). Typed
/// internally; `as_str` renders the stable event-format name at the API
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    /// Primary device attributes (`CSI c`) → `CSI ? 1 ; 2 c`.
    Da1,
    /// Secondary device attributes (`CSI > c`).
    Da2,
    /// Tertiary device attributes (`CSI = c`).
    Da3,
    /// DSR cursor position report (`CSI 6 n` → `CSI r ; c R`).
    DsrCursorPosition,
    /// DSR operating-status report (`CSI 5 n` → `CSI 0 n`).
    DsrStatus,
    /// DECRQM mode report (`CSI ? Ps $ p` → `CSI ? Ps ; 0/1 $ y`).
    Decrqm,
    /// Kitty keyboard flags query (`CSI ? u` → `CSI ? flags u`).
    KittyFlags,
    /// OSC 10/11 dynamic color query → `OSC n ; rgb:…`.
    OscColor,
}

impl QueryClass {
    /// The stable machine name used by the persisted `QueryAnswered
    /// { class }` event format (invariant 13). These exact literals are
    /// load-bearing: the audit driver matches on `"dsr_cpr"` and the MCP
    /// observe params pass `class` through verbatim.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryClass::Da1 => "da1",
            QueryClass::Da2 => "da2",
            QueryClass::Da3 => "da3",
            QueryClass::DsrCursorPosition => "dsr_cpr",
            QueryClass::DsrStatus => "dsr_status",
            QueryClass::Decrqm => "decrqm",
            QueryClass::KittyFlags => "kitty_flags",
            QueryClass::OscColor => "osc_color",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stable strings are a wire/persistence contract (invariant 13):
    /// the audit driver compares `QueryAnswered` classes against
    /// `"dsr_cpr"`, so a rename here would silently corrupt evidence.
    #[test]
    fn query_class_strings_are_stable() {
        assert_eq!(QueryClass::Da1.as_str(), "da1");
        assert_eq!(QueryClass::Da2.as_str(), "da2");
        assert_eq!(QueryClass::Da3.as_str(), "da3");
        assert_eq!(QueryClass::DsrCursorPosition.as_str(), "dsr_cpr");
        assert_eq!(QueryClass::DsrStatus.as_str(), "dsr_status");
        assert_eq!(QueryClass::Decrqm.as_str(), "decrqm");
        assert_eq!(QueryClass::KittyFlags.as_str(), "kitty_flags");
        assert_eq!(QueryClass::OscColor.as_str(), "osc_color");
    }
}
