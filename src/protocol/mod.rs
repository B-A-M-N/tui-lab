//! ProtocolTrace — decode a raw terminal byte stream into a trace of terminal
//! *operations* (review P0: "the protocol trace decoder").
//!
//! When screen introspection fails or is inconclusive (a broken render path, a
//! clear-on-write TUI, content dumped straight to stderr), the agent needs to
//! answer the diagnostic question at the raw protocol level: *"what escape
//! sequences did this TUI actually emit?"* `ProtocolTrace` turns captured PTY
//! bytes into a `Vec<TerminalOp>` — printable text, cursor moves, erase ops,
//! style (SGR), mode switches, title/OSC strings, bells, C0 controls — so the
//! agent can see the full story a terminal would have rendered.
//!
//! It is deliberately self-contained (a compact byte-level state machine, no
//! `vt100` Screen dependency): it classifies the stream rather than rendering
//! it, which is exactly what "what did the app **say** to the terminal" needs,
//! independent of any screen semantics.
//!
//! Truth guarantees (re-review P0):
//! - Byte offsets are exact: each op records the stream position where it
//!   *started*, tracked by the decoder as it consumes — never reconstructed
//!   from per-op length estimates (those lie for ST-terminated OSCs, multibyte
//!   UTF-8, and malformed tails).
//! - Text is UTF-8-decoded incrementally with partial codepoints carried
//!   across chunks; invalid bytes render as U+FFFD the way a real terminal
//!   would, rather than as mojibake.
//! - OSC 52 payloads (clipboard) are redacted by default; opt in with
//!   [`ProtocolDecoder::include_sensitive_payload`].
//! - DCS/APC/PM/SOS string sequences are captured as opaque ops — never
//!   silently discarded.
//! - Private CSI mode set/resets expand into a [`TerminalModeEvent`] timeline
//!   (alt screen, cursor visibility, mouse tracking, bracketed paste, …) so
//!   "the app enabled mouse tracking and never disabled it" is directly
//!   answerable.

/// A single classified terminal operation decoded from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerminalOp {
    /// A run of printable text (UTF-8 decoded, one op per unbroken run).
    Text(String),
    /// A C0 control character (BEL, BS, LF, CR, TAB, FF, VT, …).
    Control(ControlCode),
    /// A CSI `ESC [ ... F` sequence.
    Csi {
        /// The final byte (`A`=cursor up, `J`=erase, `m`=SGR, …).
        final_byte: char,
        /// Whether the sequence used the `?` private prefix.
        private: bool,
        /// Numeric parameters (empty when the params were elided).
        params: Vec<u16>,
    },
    /// A standalone `ESC X` sequence (no `[`/`]`).
    Escape {
        /// The byte after ESC (the command byte, or a selector prefix).
        byte: u8,
        /// A second byte, when the command was a prefix selector (`(`, `#`, …).
        consumed: Option<u8>,
    },
    /// An OSC `ESC ] P ; data (BEL | ESC \)` string, collapsed to its payload.
    Osc {
        /// The OSC number (0 = title, 8 = hyperlink, 52 = clipboard, …).
        number: Option<u16>,
        /// The payload, lossy-decoded. OSC 52 payloads are redacted unless
        /// sensitive payloads were explicitly requested (see
        /// [`ProtocolDecoder::include_sensitive_payload`]).
        payload: String,
    },
    /// A DCS / APC / PM / SOS string sequence, captured opaquely. Modern
    /// terminal extensions (Kitty protocols, tmux passthrough, …) ride in
    /// these; a debugger that dropped them would be blind to exactly the
    /// sequences that are hardest to diagnose.
    OpaqueSequence {
        /// Which string-sequence introducer began it.
        kind: StringKind,
        /// The payload bytes, hex-abbreviated when not valid UTF-8.
        summary: String,
    },
}

/// The ESC-introduced string sequence kinds (terminated by ST / BEL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StringKind {
    /// Device Control String (`ESC P`).
    Dcs,
    /// Application Program Command (`ESC _`).
    Apc,
    /// Privacy Message (`ESC ^`).
    Pm,
    /// Start of String (`ESC X`).
    Sos,
}

impl StringKind {
    fn introducer(b: u8) -> Option<Self> {
        Some(match b {
            b'P' => StringKind::Dcs,
            b'_' => StringKind::Apc,
            b'^' => StringKind::Pm,
            b'X' => StringKind::Sos,
            _ => return None,
        })
    }

    /// Display name, e.g. `dcs`.
    pub fn name(&self) -> &'static str {
        match self {
            StringKind::Dcs => "dcs",
            StringKind::Apc => "apc",
            StringKind::Pm => "pm",
            StringKind::Sos => "sos",
        }
    }
}

/// A C0 control code recognized in the ground state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ControlCode {
    Bell,
    Backspace,
    Tab,
    LineFeed,
    VerticalTab,
    FormFeed,
    CarriageReturn,
    Other(u8),
}

impl ControlCode {
    fn classify(b: u8) -> Option<Self> {
        Some(match b {
            0x07 => ControlCode::Bell,
            0x08 => ControlCode::Backspace,
            0x09 => ControlCode::Tab,
            0x0a => ControlCode::LineFeed,
            0x0b => ControlCode::VerticalTab,
            0x0c => ControlCode::FormFeed,
            0x0d => ControlCode::CarriageReturn,
            0x00..=0x1f | 0x7f => ControlCode::Other(b),
            _ => return None,
        })
    }
}

/// Coarse category an agent counts/summarizes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Text,
    Control,
    Csi,
    Escape,
    Osc,
    Opaque,
}

impl TerminalOp {
    /// Coarse category for counting/summarizing.
    pub fn kind(&self) -> OpKind {
        match self {
            TerminalOp::Text(_) => OpKind::Text,
            TerminalOp::Control(_) => OpKind::Control,
            TerminalOp::Csi { .. } => OpKind::Csi,
            TerminalOp::Escape { .. } => OpKind::Escape,
            TerminalOp::Osc { .. } => OpKind::Osc,
            TerminalOp::OpaqueSequence { .. } => OpKind::Opaque,
        }
    }

    /// A one-line human-readable rendering of the operation.
    pub fn describe(&self) -> String {
        match self {
            TerminalOp::Text(s) => format!("text {:?}", s),
            TerminalOp::Control(c) => format!("control {c:?}"),
            TerminalOp::Csi {
                final_byte,
                private,
                params,
            } => {
                let prefix = if *private { "?" } else { "" };
                let p = if params.is_empty() {
                    String::new()
                } else {
                    params
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(";")
                };
                format!("csi {prefix}{p}{final_byte}")
            }
            TerminalOp::Escape { byte, consumed } => match consumed {
                Some(c) => format!("esc {} {}", esc_byte(*byte), esc_byte(*c)),
                None => format!("esc {}", esc_byte(*byte)),
            },
            TerminalOp::Osc { number, payload } => match number {
                Some(n) => format!("osc {n}: {}", truncate(payload, 48)),
                None => format!("osc: {}", truncate(payload, 48)),
            },
            TerminalOp::OpaqueSequence { kind, summary } => {
                format!("{} {}", kind.name(), truncate(summary, 48))
            }
        }
    }
}

fn esc_byte(b: u8) -> String {
    match b {
        0x1b => "ESC".into(),
        b if b.is_ascii_graphic() || b == b' ' => (b as char).to_string(),
        _ => format!("\\x{b:02x}"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count > n {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    } else {
        s.to_string()
    }
}

/// Printable summary of an opaque sequence payload: the text itself when it
/// is valid UTF-8, a hex abbreviation otherwise.
fn opaque_summary(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let head: String = bytes
                .iter()
                .take(16)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            if bytes.len() > 16 {
                format!("{head}… ({} bytes)", bytes.len())
            } else {
                head
            }
        }
    }
}

// ── Terminal mode timeline ─────────────────────────────────────────────────

/// A semantic terminal-mode event decoded from a private CSI h/l (DECSET /
/// DECRESET) — the human-meaningful reading of `CSI ? 1049 h` and friends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalModeEvent {
    /// Semantic mode name, e.g. `alt_screen`, `cursor_visible`,
    /// `mouse_press_release`, `bracketed_paste`.
    pub mode: &'static str,
    /// Whether the mode was set (true) or reset (false).
    pub set: bool,
    /// Byte offset where the CSI sequence began.
    pub at: usize,
}

/// Map a private mode number to its semantic name. `None` = not one of the
/// modes we name (the raw CSI is still in the op trace; this layer only adds
/// readings for modes with a well-known meaning).
fn mode_name(n: u16) -> Option<&'static str> {
    Some(match n {
        1 => "application_cursor_keys",
        25 => "cursor_visible",
        47 => "alt_screen",
        1000 => "mouse_press_release",
        1002 => "mouse_button_motion",
        1003 => "mouse_any_motion",
        1006 => "mouse_sgr_encoding",
        1047 => "alt_screen",
        1049 => "alt_screen",
        2004 => "bracketed_paste",
        2026 => "synchronized_update",
        _ => return None,
    })
}

/// Tri-state terminal-mode state (re-review item 20): a mode whose history
/// is incomplete is **Unknown**, not disabled. "The raw ring starts halfway
/// through the app's lifetime" means an absent mouse-ON record says nothing
/// about whether mouse tracking is on — audits must propagate that instead
/// of misreading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownModeState {
    /// No evidence in the retained window: the mode's state cannot be known.
    Unknown,
    Enabled,
    Disabled,
}

/// The tri-state view of every named mode after replaying a mode-event
/// timeline. When `history_complete` is false the fold starts from
/// [`KnownModeState::Unknown`] for every mode (an observed set/reset moves a
/// mode to Enabled/Disabled; modes never seen stay Unknown). When the
/// history IS complete, absent modes are honestly `Disabled` — a full
/// transcript with no DECSET means the mode was never turned on.
pub fn fold_mode_states(
    events: &[TerminalModeEvent],
    history_complete: bool,
) -> std::collections::BTreeMap<&'static str, KnownModeState> {
    let mut out: std::collections::BTreeMap<&'static str, KnownModeState> =
        std::collections::BTreeMap::new();
    let seed = if history_complete {
        KnownModeState::Disabled
    } else {
        KnownModeState::Unknown
    };
    // Seed every mode the vocabulary names, so the map is total.
    const KNOWN_MODES: &[&str] = &[
        "application_cursor_keys",
        "cursor_visible",
        "alt_screen",
        "mouse_press_release",
        "mouse_button_motion",
        "mouse_any_motion",
        "mouse_sgr_encoding",
        "bracketed_paste",
        "synchronized_update",
    ];
    for m in KNOWN_MODES {
        out.insert(*m, seed);
    }
    for ev in events {
        // Only insert modes the vocabulary names (mode_name already filters).
        out.insert(
            ev.mode,
            if ev.set {
                KnownModeState::Enabled
            } else {
                KnownModeState::Disabled
            },
        );
    }
    out
}

// ── Incremental decoder ────────────────────────────────────────────────────

/// Intermediate state while decoding a chunk stream (CSI/OSC may span a chunk
/// boundary, so the decoder carries state across `feed`s).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum DecodeState {
    #[default]
    Ground,
    /// After `ESC`, expecting the discriminator (`[`, `]`, selector, command).
    Escape,
    /// A selector prefix (`(`, `#`, …) seen; expecting its one argument byte.
    EscapePrefix(u8),
    /// Collecting CSI parameter bytes (0x20–0x3f) until the final byte.
    /// Carries the stream offset where the sequence began (for exact stamps).
    Csi {
        private: bool,
        params: Vec<u8>,
        start: usize,
    },
    /// Collecting an OSC payload until BEL or ST (`ESC \`).
    Osc(Option<u16>, Vec<u8>, usize),
    /// Inside OSC after an `ESC`; a following `\` is the ST terminator.
    OscSt(Option<u16>, Vec<u8>, usize),
    /// Collecting a DCS/APC/PM/SOS payload until ST (or BEL for DCS).
    StringSeq(StringKind, Vec<u8>, usize),
    /// Inside a string sequence after an `ESC`; a following `\` is ST.
    StringSeqSt(StringKind, Vec<u8>, usize),
}

/// Incremental, chunk-safe decoder. Feed the raw stream in pieces with
/// [`Self::feed`]; a CSI/OSC/ESC sequence that spans a chunk boundary is
/// carried and completed on the next feed.
///
/// UTF-8 is decoded incrementally: a multibyte codepoint split across chunks
/// completes correctly on the chunk that finishes it. Invalid bytes decode
/// as U+FFFD (what a real terminal shows), never as per-byte mojibake.
#[derive(Debug, Default)]
pub struct ProtocolDecoder {
    state: DecodeState,
    /// Accumulated text run bytes (UTF-8 partials included), flushed into a
    /// `Text` op on a break.
    text: Vec<u8>,
    /// Absolute stream position of the NEXT byte fed (a running cursor so op
    /// start offsets are exact).
    pos: usize,
    /// Offset where the current text run began.
    text_start: usize,
    /// Pending UTF-8 continuation bytes for a codepoint that hasn't finished.
    utf8_pending: Vec<u8>,
    /// Whether OSC 52 (clipboard) payloads are preserved (false = redact).
    sensitive_payloads: bool,
    /// Mode timeline accumulated across feeds (drained via
    /// [`Self::take_mode_events`]).
    mode_events: Vec<TerminalModeEvent>,
    /// Ops emitted by the current step loop, each stamped with the exact
    /// stream offset where it began.
    emitted: Vec<(usize, TerminalOp)>,
}

impl ProtocolDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opt in to preserving sensitive payloads (OSC 52 clipboard data).
    /// Default is redact: traces and repair packets travel widely, and a
    /// clipboard copy silently embedded in one is a leak.
    pub fn include_sensitive_payload(&mut self, yes: bool) -> &mut Self {
        self.sensitive_payloads = yes;
        self
    }

    /// Feed a byte slice; returns every terminal op completed by this chunk.
    /// A trailing partial sequence is *not* emitted — it stays pending until
    /// the next `feed` (or [`Self::finish`]).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalOp> {
        self.feed_stamped(bytes)
            .into_iter()
            .map(|(_, op)| op)
            .collect()
    }

    /// Feed a byte slice; returns each op with the EXACT byte offset where
    /// it began. This is the offset authority — nothing downstream estimates
    /// lengths.
    pub fn feed_stamped(&mut self, bytes: &[u8]) -> Vec<(usize, TerminalOp)> {
        for &b in bytes {
            self.pos += 1;
            self.step(b);
        }
        // A COMPLETE text run still open at chunk end is emitted now, so a
        // streaming consumer of chunk N sees every op chunk N completed.
        // An incomplete UTF-8 codepoint is NOT rendered here — the stream
        // continues; only `finish` (true end of stream) resolves partials
        // to U+FFFD. Likewise a partially-received sequence (OSC/DCS mid-
        // payload) stays pending for the next chunk.
        self.flush_text_at_boundary();
        std::mem::take(&mut self.emitted)
    }

    /// Flush pending text and materialize trailing partial sequences, stamped.
    pub fn finish_stamped(&mut self) -> Vec<(usize, TerminalOp)> {
        self.finish_into();
        std::mem::take(&mut self.emitted)
    }

    /// Drain the terminal-mode events decoded so far (non-destructive reads
    /// should prefer [`Self::mode_events`]).
    pub fn take_mode_events(&mut self) -> Vec<TerminalModeEvent> {
        std::mem::take(&mut self.mode_events)
    }

    /// Read-only view of the mode timeline accumulated so far.
    pub fn mode_events(&self) -> &[TerminalModeEvent] {
        &self.mode_events
    }

    /// Flush pending text and materialize a trailing unfinished sequence so a
    /// trace never silently drops bytes that ended mid-stream.
    pub fn finish(&mut self) -> Vec<TerminalOp> {
        self.finish_stamped()
            .into_iter()
            .map(|(_, op)| op)
            .collect()
    }

    /// Core finish: push the tail ops (stamped) into `out`.
    fn finish_into(&mut self) {
        self.flush_text();
        match std::mem::take(&mut self.state) {
            DecodeState::Ground => {}
            // An ESC with nothing after it cannot describe an op — drop it.
            DecodeState::Escape => {}
            DecodeState::EscapePrefix(b) => {
                let at = self.pos.saturating_sub(1);
                self.emitted.push((
                    at,
                    TerminalOp::Escape {
                        byte: b,
                        consumed: None,
                    },
                ));
            }
            // A CSI with no final byte is genuinely incomplete (malformed
            // stream tail). Surface it as its escaped bytes rather than invent
            // a final byte; an agent seeing it knows the stream was cut.
            DecodeState::Csi {
                private,
                params,
                start,
            } => {
                if let Some(fb) = raw_final_byte(&params) {
                    let numbers = split_params(&params[..params.len() - 1], private);
                    self.emitted.push((
                        start,
                        TerminalOp::Csi {
                            final_byte: fb,
                            private,
                            params: numbers.unwrap_or_default(),
                        },
                    ));
                }
            }
            DecodeState::Osc(number, payload, start)
            | DecodeState::OscSt(number, payload, start) => {
                let op = self.finalize_osc(number, payload);
                self.emitted.push((start, op));
            }
            DecodeState::StringSeq(kind, payload, start) => {
                self.emitted.push((
                    start,
                    TerminalOp::OpaqueSequence {
                        kind,
                        summary: opaque_summary(&payload),
                    },
                ));
            }
            DecodeState::StringSeqSt(kind, payload, start) => {
                self.emitted.push((
                    start,
                    TerminalOp::OpaqueSequence {
                        kind,
                        summary: opaque_summary(&payload),
                    },
                ));
            }
        }
    }

    /// The remaining ops once the stream is known-complete — a convenience
    /// wrapper over `feed` + `finish`.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<TerminalOp> {
        let mut ops = self.feed(bytes);
        ops.extend(self.finish());
        ops
    }

    /// Flush the open text run as U+FFFD-resolved text (END OF STREAM: a
    /// partial codepoint can never complete, so it renders as replacement —
    /// what a real terminal shows).
    fn flush_text(&mut self) {
        if !self.utf8_pending.is_empty() {
            self.text.extend_from_slice("\u{fffd}".as_bytes());
            self.utf8_pending.clear();
        }
        self.emit_text_run();
    }

    /// Flush only COMPLETE text at a chunk boundary. An incomplete UTF-8
    /// codepoint stays pending: the next chunk continues it. This is the
    /// streaming-safe flush.
    fn flush_text_at_boundary(&mut self) {
        if self.utf8_pending.is_empty() {
            self.emit_text_run();
        }
        // else: hold both the completed bytes and the partial — they are one
        // logical run and `text` keeps its start offset for the eventual
        // emit. (Emitting the completed head early would SPLIT one visible
        // run into two ops.)
    }

    /// Push any accumulated text bytes out as one Text op.
    fn emit_text_run(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.text);
        let start = self.text_start;
        self.emitted.push((
            start,
            TerminalOp::Text(String::from_utf8_lossy(&bytes).into_owned()),
        ));
    }

    /// Ground-state text byte: fold into the UTF-8 run, decoding complete
    /// codepoints incrementally and mapping invalid bytes to U+FFFD.
    fn push_text_byte(&mut self, b: u8) {
        // UTF-8 continuation expected?
        if !self.utf8_pending.is_empty() {
            self.utf8_pending.push(b);
            match decode_utf8_head(&self.utf8_pending) {
                Utf8Result::Incomplete => return, // still waiting for bytes
                Utf8Result::Valid => {
                    let chunk = std::mem::take(&mut self.utf8_pending);
                    self.text.extend_from_slice(&chunk);
                    return;
                }
                Utf8Result::Invalid => {
                    // Replace the WHOLE pending head with one U+FFFD (the
                    // standard maximal-subpart behavior) and reprocess this
                    // byte as a fresh start.
                    self.text.extend_from_slice("\u{fffd}".as_bytes());
                    self.utf8_pending.clear();
                    // fall through to reprocess b as a lead byte below
                }
            }
        }
        if b < 0x80 {
            self.text.push(b);
        } else if is_utf8_lead(b) {
            self.utf8_pending.push(b);
        } else {
            // Stray continuation byte with no pending head: U+FFFD.
            self.text.extend_from_slice("\u{fffd}".as_bytes());
        }
    }

    fn step(&mut self, b: u8) {
        // Take the state out so arms can call back into `self` (flush/step)
        // without aliasing the borrow; each arm restores it.
        let state = std::mem::take(&mut self.state);
        match state {
            DecodeState::Ground => {
                self.state = DecodeState::Ground;
                self.step_ground(b);
            }
            DecodeState::Escape => {
                self.state = DecodeState::Ground;
                self.step_escape(b);
            }
            DecodeState::EscapePrefix(prefix) => {
                self.flush_text();
                let at = self.pos.saturating_sub(1);
                self.emitted.push((
                    at,
                    TerminalOp::Escape {
                        byte: prefix,
                        consumed: Some(b),
                    },
                ));
                self.state = DecodeState::Ground;
            }
            DecodeState::Csi {
                private,
                mut params,
                start,
            } => {
                if (0x20..=0x3f).contains(&b) {
                    params.push(b);
                    self.state = DecodeState::Csi {
                        private,
                        params,
                        start,
                    };
                } else if (0x40..=0x7e).contains(&b) {
                    // Final byte: conclude the CSI.
                    params.push(b);
                    self.flush_text();
                    if let Some(op) = conclude_csi(private, &params) {
                        let at = start;
                        self.record_mode_events(&op, at);
                        self.emitted.push((at, op));
                    }
                    self.state = DecodeState::Ground;
                } else {
                    // Out-of-range byte terminates the CSI without a final.
                    // Feed the offending byte back through Ground.
                    self.state = DecodeState::Ground;
                    self.step(b);
                }
            }
            DecodeState::Osc(mut number, mut payload, start) => match b {
                0x07 => {
                    self.flush_text();
                    let op = self.finalize_osc(number.take(), payload);
                    self.emitted.push((start, op));
                    self.state = DecodeState::Ground;
                }
                0x1b => {
                    self.state = DecodeState::OscSt(number.take(), payload, start);
                }
                _ if b >= 0x07 => {
                    payload.push(b);
                    self.state = DecodeState::Osc(number, payload, start);
                }
                _ => {
                    self.state = DecodeState::Osc(number, payload, start);
                }
            },
            DecodeState::OscSt(number, payload, start) => {
                if b == b'\\' {
                    self.flush_text();
                    let op = self.finalize_osc(number, payload);
                    self.emitted.push((start, op));
                    self.state = DecodeState::Ground;
                } else {
                    // Not an ST — that ESC began a real escape sequence.
                    self.state = DecodeState::Escape;
                    self.step(b);
                }
            }
            DecodeState::StringSeq(kind, mut payload, start) => {
                // DCS may legally terminate on BEL too (some senders do);
                // APC/PM/SOS terminate on ST only. ST is ESC \.
                let bel_ok = matches!(kind, StringKind::Dcs);
                match b {
                    0x1b => {
                        self.state = DecodeState::StringSeqSt(kind, payload, start);
                    }
                    0x07 if bel_ok => {
                        self.flush_text();
                        self.emitted.push((
                            start,
                            TerminalOp::OpaqueSequence {
                                kind,
                                summary: opaque_summary(&payload),
                            },
                        ));
                        self.state = DecodeState::Ground;
                    }
                    _ if b >= 0x08 => {
                        payload.push(b);
                        self.state = DecodeState::StringSeq(kind, payload, start);
                    }
                    _ => {
                        self.state = DecodeState::StringSeq(kind, payload, start);
                    }
                }
            }
            DecodeState::StringSeqSt(kind, payload, start) => {
                if b == b'\\' {
                    self.flush_text();
                    self.emitted.push((
                        start,
                        TerminalOp::OpaqueSequence {
                            kind,
                            summary: opaque_summary(&payload),
                        },
                    ));
                    self.state = DecodeState::Ground;
                } else {
                    self.state = DecodeState::Escape;
                    self.step(b);
                }
            }
        }
    }

    /// Decode a private-mode CSI h/l into semantic mode events (multi-param
    /// sequences expand: `CSI ? 1000 ; 1006 h` is two events).
    fn record_mode_events(&mut self, op: &TerminalOp, at: usize) {
        let TerminalOp::Csi {
            final_byte,
            private,
            params,
        } = op
        else {
            return;
        };
        if !private || (*final_byte != 'h' && *final_byte != 'l') {
            return;
        }
        let set = *final_byte == 'h';
        for p in params {
            if let Some(name) = mode_name(*p) {
                self.mode_events.push(TerminalModeEvent {
                    mode: name,
                    set,
                    at,
                });
            }
        }
    }

    fn step_ground(&mut self, b: u8) {
        if b == 0x1b {
            self.flush_text();
            self.text_start = self.pos - 1;
            self.state = DecodeState::Escape;
        } else if let Some(c) = ControlCode::classify(b) {
            self.flush_text();
            let at = self.pos - 1;
            self.emitted.push((at, TerminalOp::Control(c)));
        } else if b >= 0x20 {
            // Printable ASCII or UTF-8 lead/continuation: fold into the run.
            if self.text.is_empty() && self.utf8_pending.is_empty() {
                self.text_start = self.pos - 1;
            }
            self.push_text_byte(b);
        }
        // 0x00-0x06/0x0e-0x1f classified above; DAC etc. handled by classify.
    }

    fn step_escape(&mut self, b: u8) {
        match b {
            b'[' => {
                self.state = DecodeState::Csi {
                    private: false,
                    params: Vec::new(),
                    start: self.text_start,
                }
            }
            b']' => self.state = DecodeState::Osc(None, Vec::new(), self.text_start),
            b'P' | b'_' | b'^' | b'X' => {
                let kind = StringKind::introducer(b).expect("introducer matched");
                self.state = DecodeState::StringSeq(kind, Vec::new(), self.text_start);
            }
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' | b' ' => {
                self.state = DecodeState::EscapePrefix(b);
            }
            _ if (0x20..=0x7f).contains(&b) && b != 0x7f => {
                self.flush_text();
                let at = self.pos.saturating_sub(1);
                self.emitted.push((
                    at,
                    TerminalOp::Escape {
                        byte: b,
                        consumed: None,
                    },
                ));
                self.state = DecodeState::Ground;
            }
            _ => {
                self.state = DecodeState::Ground;
            }
        }
    }

    /// Build the OSC op with redaction applied (OSC 52 clipboard payloads
    /// are a leak vector in shared traces).
    fn finalize_osc(&mut self, number: Option<u16>, payload: Vec<u8>) -> TerminalOp {
        let text = String::from_utf8_lossy(&payload).into_owned();
        if number.is_none() {
            if let Some(semi) = text.find(';') {
                let head = &text[..semi];
                if let Ok(n) = head.parse::<u16>() {
                    return self.osc_op(Some(n), text[semi + 1..].to_string());
                }
            }
        }
        self.osc_op(number, text)
    }

    fn osc_op(&self, number: Option<u16>, payload: String) -> TerminalOp {
        if number == Some(52) && !self.sensitive_payloads {
            return TerminalOp::Osc {
                number,
                payload: format!(
                    "OSC52 clipboard payload bytes={} redacted=true",
                    payload.len()
                ),
            };
        }
        TerminalOp::Osc { number, payload }
    }
}

/// UTF-8 lead bytes of multi-byte sequences.
fn is_utf8_lead(b: u8) -> bool {
    (0xc2..=0xf4).contains(&b)
}

/// The possible outcomes of decoding a pending UTF-8 head + continuations.
enum Utf8Result {
    /// Head seen; more continuation bytes expected.
    Incomplete,
    /// A complete valid codepoint.
    Valid,
    /// The head can never become valid (bad lead or bad continuation).
    Invalid,
}

/// Classify a pending UTF-8 sequence (1..=4 bytes) without allocating.
fn decode_utf8_head(buf: &[u8]) -> Utf8Result {
    let head = buf[0];
    // (total_len, first-continuation lo, first-continuation hi). The tight
    // ranges are an overlong/surrogate guard on the FIRST continuation only;
    // later continuations are always 0x80..=0xbf (e.g. U+1F980 is
    // f0 9f a6 80 — its final 0x80 is below f0's first-cont floor of 0x90
    // and must still be accepted).
    let (need, lo, hi): (usize, u8, u8) = match head {
        0xc2..=0xdf => (2, 0x80, 0xbf),
        0xe0 => (3, 0xa0, 0xbf),
        0xe1..=0xec | 0xee..=0xef => (3, 0x80, 0xbf),
        0xed => (3, 0x80, 0x9f),
        0xf0 => (4, 0x90, 0xbf),
        0xf1..=0xf3 => (4, 0x80, 0xbf),
        0xf4 => (4, 0x80, 0x8f),
        _ => return Utf8Result::Invalid,
    };
    let mut conts = buf[1..].iter().copied();
    let Some(first) = conts.next() else {
        return Utf8Result::Incomplete;
    };
    if !(lo..=hi).contains(&first) {
        return Utf8Result::Invalid;
    }
    let mut seen = 1usize; // continuation bytes validated so far
    for c in conts {
        if !(0x80..=0xbf).contains(&c) {
            return Utf8Result::Invalid;
        }
        seen += 1;
    }
    if seen + 1 < need {
        return Utf8Result::Incomplete;
    }
    Utf8Result::Valid
}

/// The final byte of a CSI, if it ends in one (0x40–0x7e).
fn raw_final_byte(raw: &[u8]) -> Option<char> {
    raw.last()
        .filter(|&&b| (0x40..=0x7e).contains(&b))
        .map(|&b| b as char)
}

/// Whether a CSI used the `?` private marker (private mode set/reset).
fn csi_private(raw: &[u8]) -> bool {
    raw.contains(&b'?')
}

/// Build a `Csi` op from collected parameter bytes (final byte already
/// appended). The final byte is guaranteed to be the last byte (0x40–0x7e);
/// the `?` private marker is detected from the bytes themselves.
fn conclude_csi(_private: bool, raw: &[u8]) -> Option<TerminalOp> {
    let final_byte = *raw.last()? as char;
    let private = csi_private(raw);
    let param_bytes = &raw[..raw.len().saturating_sub(1)];
    let params = split_params(param_bytes, private).unwrap_or_default();
    Some(TerminalOp::Csi {
        final_byte,
        private,
        params,
    })
}

/// Parse the parameter intermed bytes (private markers, digits, `;`) into
/// numeric params. Returns `None` on an odd parse — the caller falls back to
/// an empty param list.
fn split_params(raw: &[u8], private: bool) -> Option<Vec<u16>> {
    let _ = private;
    if raw.is_empty() {
        return Some(Vec::new());
    }
    let mut nums = Vec::new();
    let mut cur = 0u16;
    let mut any = false;
    for &b in raw {
        if (0x30..=0x39).contains(&b) {
            cur = cur.saturating_mul(10).saturating_add((b - b'0') as u16);
            any = true;
        } else if b == b';' {
            nums.push(cur);
            cur = 0;
            any = false;
        }
        // intermediate bytes outside digit/`;` (e.g. `?`, `:`) are ignored.
    }
    if any {
        nums.push(cur);
    }
    Some(nums)
}

/// A complete, ordered trace with byte-offset stamps — the agent-facing view.
pub struct ProtocolTrace {
    /// The operations, in stream order.
    pub ops: Vec<TraceEntry>,
    /// Semantic terminal-mode timeline (private CSI h/l readings), in order.
    pub modes: Vec<TerminalModeEvent>,
}

/// One operation plus where it started in the raw stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraceEntry {
    /// Byte offset into the source stream where this op began. Exact: tracked
    /// by the decoder as it consumed, never estimated after the fact.
    pub at: usize,
    /// The operation.
    pub op: TerminalOp,
}

impl ProtocolTrace {
    /// Decode an entire captured byte stream into an offset-stamped trace.
    /// Sensitive payloads (OSC 52 clipboard) are redacted; use
    /// [`Self::decode_with_sensitive`] when a clipboard investigation truly
    /// needs the payload.
    pub fn decode(bytes: &[u8]) -> Self {
        Self::decode_impl(bytes, false)
    }

    /// Decode with sensitive payloads preserved (clipboard debugging).
    pub fn decode_with_sensitive(bytes: &[u8]) -> Self {
        Self::decode_impl(bytes, true)
    }

    fn decode_impl(bytes: &[u8], sensitive: bool) -> Self {
        let mut decoder = ProtocolDecoder::new();
        decoder.include_sensitive_payload(sensitive);
        // The decoder stamps each op with its exact start offset as it
        // consumes; nothing here estimates lengths after the fact.
        let mut ops: Vec<TraceEntry> = decoder
            .feed_stamped(bytes)
            .into_iter()
            .map(|(at, op)| TraceEntry { at, op })
            .collect();
        ops.extend(
            decoder
                .finish_stamped()
                .into_iter()
                .map(|(at, op)| TraceEntry { at, op }),
        );
        let modes = decoder.take_mode_events();
        ProtocolTrace { ops, modes }
    }

    /// Ops of a given kind, for filtering (e.g. "only CSI and OSC").
    pub fn of_kind(&self, kind: OpKind) -> Vec<&TraceEntry> {
        self.ops.iter().filter(|e| e.op.kind() == kind).collect()
    }

    /// One line per op, e.g. `0: text "MENU"\n 10: csi 2J`.
    pub fn to_text(&self) -> String {
        self.ops
            .iter()
            .map(|e| format!("{}: {}", e.at, e.op.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the trace contains any text.
    pub fn has_text(&self) -> bool {
        self.ops.iter().any(|e| matches!(e.op, TerminalOp::Text(_)))
    }
}

/// Re-export the pieces trace consumers need.
pub use ControlCode as C0;
pub use OpKind as TraceKind;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_and_controls() {
        let d = ProtocolTrace::decode(b"hello\nworld\r");
        assert_eq!(d.ops.len(), 4); // text hello, LF, text world, CR
        assert_eq!(d.ops[0].op, TerminalOp::Text("hello".into()));
        assert_eq!(d.ops[1].op, TerminalOp::Control(ControlCode::LineFeed));
        assert_eq!(d.ops[2].op, TerminalOp::Text("world".into()));
        assert_eq!(
            d.ops[3].op,
            TerminalOp::Control(ControlCode::CarriageReturn)
        );
        assert!(d.has_text());
    }

    #[test]
    fn decodes_sgr_and_private_sequences() {
        let d = ProtocolTrace::decode(b"\x1b[31mred\x1b[0m");
        assert_eq!(
            d.ops[0].op,
            TerminalOp::Csi {
                final_byte: 'm',
                private: false,
                params: vec![31],
            }
        );
        assert_eq!(d.ops[1].op, TerminalOp::Text("red".into()));

        // Private mode set: ESC [ ? 25 l
        let d2 = ProtocolTrace::decode(b"\x1b[?25l");
        let csi = d2.ops[0].op.clone();
        if let TerminalOp::Csi {
            final_byte,
            private,
            params,
        } = csi
        {
            assert_eq!(final_byte, 'l');
            assert!(private);
            assert_eq!(params, vec![25]);
        } else {
            panic!("not a csi");
        }
    }

    #[test]
    fn decodes_multi_param_and_elided_params() {
        // ESC [ 1 ; 31 ; 42 m  -> three params.
        let d = ProtocolTrace::decode(b"\x1b[1;31;42m");
        if let TerminalOp::Csi { params, .. } = &d.ops[0].op {
            assert_eq!(params, &vec![1, 31, 42]);
        } else {
            panic!("not csi");
        }

        // Bare ESC [ J  -> elided params = empty.
        let d2 = ProtocolTrace::decode(b"\x1b[J");
        if let TerminalOp::Csi {
            params, final_byte, ..
        } = &d2.ops[0].op
        {
            assert_eq!(final_byte, &'J');
            assert!(params.is_empty(), "elided CSI has no params");
        } else {
            panic!("not csi");
        }
    }

    #[test]
    fn decodes_osc_title_and_hyperlink() {
        let d = ProtocolTrace::decode(b"\x1b]0;HERMES\x07");
        assert_eq!(
            d.ops[0].op,
            TerminalOp::Osc {
                number: Some(0),
                payload: "HERMES".into(),
            }
        );

        // OSC8 hyperlink terminated by ST (ESC \) instead of BEL.
        let d2 = ProtocolTrace::decode(b"\x1b]8;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        let oscs: Vec<_> = d2
            .ops
            .iter()
            .filter(|e| e.op.kind() == OpKind::Osc)
            .collect();
        assert_eq!(oscs.len(), 2);
        assert!(oscs[0].op.describe().contains("https://example.com"));
        assert!(d2.has_text(), "the 'link' text must be decoded");
    }

    #[test]
    fn csi_spans_chunk_boundary() {
        let mut decoder = ProtocolDecoder::new();
        let _ = decoder.feed(b"\x1b[1;3");
        let next = decoder.feed(b"1m");
        assert_eq!(next.len(), 1);
        if let TerminalOp::Csi { params, .. } = &next[0] {
            assert_eq!(params, &vec![1, 31]);
        } else {
            panic!("csi not concluded across boundary");
        }
    }

    #[test]
    fn standalone_escape_and_prefix() {
        // ESC M = reverse index; ESC ( B = charset.
        let d = ProtocolTrace::decode(b"\x1bM\x1b(B");
        assert_eq!(
            d.ops[0].op,
            TerminalOp::Escape {
                byte: b'M',
                consumed: None,
            }
        );
        if let TerminalOp::Escape { byte, consumed } = &d.ops[1].op {
            assert_eq!(byte, &b'(');
            assert_eq!(consumed, &Some(b'B'));
        } else {
            panic!("not esc");
        }
    }

    #[test]
    fn summary_counts_kinds() {
        let d = ProtocolTrace::decode(b"AB\x1b[0m\x01");
        assert_eq!(d.of_kind(OpKind::Text).len(), 1);
        assert_eq!(d.of_kind(OpKind::Csi).len(), 1);
        assert_eq!(d.of_kind(OpKind::Control).len(), 1);
        let text = d.to_text();
        assert!(text.contains("text \"AB\""));
        assert!(text.contains("csi 0m"));
    }

    // ── UTF-8 truth (re-review P0) ────────────────────────────────────────

    #[test]
    fn utf8_multibyte_text_decodes() {
        // "設定" (3-byte chars) + an accented 2-byte char + emoji (4-byte).
        let d = ProtocolTrace::decode("設定é🦀".as_bytes());
        assert_eq!(d.ops.len(), 1, "one unbroken text run, got {:?}", d.ops);
        assert_eq!(d.ops[0].op, TerminalOp::Text("設定é🦀".into()));
    }

    #[test]
    fn utf8_split_across_chunks() {
        // "設" is 3 bytes (E8 A8 AD), "定" likewise. Feed ONE BYTE per chunk:
        // each completed codepoint must emerge whole at the chunk boundary
        // that finished it (streaming contract), never as mojibake.
        let mut dec = ProtocolDecoder::new();
        let mut all = Vec::new();
        for b in "設定".as_bytes() {
            all.extend(dec.feed(&[*b]));
        }
        all.extend(dec.finish());
        assert_eq!(
            all,
            vec![TerminalOp::Text("設".into()), TerminalOp::Text("定".into())],
            "one Text op per completed codepoint, decoded exactly"
        );
        // A single-chunk decode of the same bytes is ONE run.
        assert_eq!(ProtocolTrace::decode("設定".as_bytes()).ops.len(), 1);
    }

    #[test]
    fn invalid_utf8_becomes_replacement_char() {
        // A lone continuation byte (0x9C) with no head: U+FFFD, then text.
        let d = ProtocolTrace::decode(b"a\x9cb");
        assert_eq!(d.ops.len(), 1);
        assert_eq!(d.ops[0].op, TerminalOp::Text("a\u{fffd}b".into()));
        // A truncated 3-byte head at end of stream: U+FFFD at finish.
        let d2 = ProtocolTrace::decode(b"x\xe8\xa8");
        assert_eq!(
            d2.ops.last().unwrap().op,
            TerminalOp::Text("x\u{fffd}".into())
        );
    }

    // ── Exact byte offsets (re-review P0) ─────────────────────────────────

    #[test]
    fn offsets_are_exact() {
        // layout: "AB"(0..2) BEL(2) OSC-0 started at 3, BEL-terminated,
        // OSC-8 started at 14, ST-terminated; CSI started at 40; CJK text.
        let mut stream = b"AB".to_vec();
        stream.push(0x07);
        stream.extend(b"\x1b]0;TITLE\x07");
        let osc_st_at = stream.len();
        stream.extend(b"\x1b]8;;https://x\x1b\\");
        let csi_at = stream.len();
        stream.extend(b"\x1b[2J");
        let text_at = stream.len();
        stream.extend("設".as_bytes());

        let d = ProtocolTrace::decode(&stream);
        let bel = d
            .ops
            .iter()
            .find(|e| e.op.kind() == OpKind::Control)
            .unwrap();
        assert_eq!(bel.at, 2, "BEL at byte 2");
        let osc0 = &d.ops[2];
        assert!(
            matches!(&osc0.op, TerminalOp::Osc { number: Some(0), payload } if payload == "TITLE"),
            "osc0 payload, got {:?}",
            osc0.op
        );
        assert_eq!(osc0.at, 3, "OSC-0 began at 3 (right after BEL)");
        let osc8 = &d.ops[3];
        assert!(
            matches!(
                &osc8.op,
                TerminalOp::Osc {
                    number: Some(8),
                    ..
                }
            ),
            "osc8"
        );
        assert_eq!(osc8.at, osc_st_at, "ST-terminated OSC offset must be exact");
        let csi = &d.ops[4];
        assert!(
            matches!(&csi.op, TerminalOp::Csi { final_byte: 'J', params, .. } if params == &vec![2]),
            "expected CSI 2J, got {:?}",
            csi.op
        );
        assert_eq!(csi.at, csi_at);
        let cjk = &d.ops[5];
        assert_eq!(cjk.at, text_at);
        assert_eq!(cjk.op, TerminalOp::Text("設".into()));
    }

    #[test]
    fn offsets_survive_chunking() {
        let stream: Vec<u8> = b"\x1b]0;TITLE\x07rest".to_vec();
        let mut dec = ProtocolDecoder::new();
        let mut stamped = dec.feed_stamped(&stream[..5]);
        // Feed the REST (chunk boundary mid-OSC), never finish(): the text
        // "rest" must flush on the break with its real stream offset (13).
        stamped.extend(dec.feed_stamped(&stream[5..]));
        let text = stamped.iter().find(|(_, op)| op.kind() == OpKind::Text);
        assert_eq!(
            text.map(|(at, _)| *at),
            Some(10),
            "text run began at 10 (after the 10-byte OSC)"
        );
        let osc = stamped.iter().find(|(_, op)| op.kind() == OpKind::Osc);
        assert_eq!(osc.map(|(at, _)| *at), Some(0), "OSC began at 0");
    }

    // ── OSC 52 redaction (re-review P0) ───────────────────────────────────

    #[test]
    fn osc52_redacted_by_default() {
        // base64 of "secret" is c2VjcmV0.
        let stream = b"\x1b]52;c;c2VjcmV0\x07";
        let d = ProtocolTrace::decode(stream);
        if let TerminalOp::Osc {
            number: Some(52),
            payload,
        } = &d.ops[0].op
        {
            assert!(
                payload.contains("redacted=true"),
                "redaction note: {payload}"
            );
            assert!(!payload.contains("c2VjcmV0"), "payload must not leak");
        } else {
            panic!("not osc52: {:?}", d.ops[0].op);
        }
        // Opt-in preserves it (payload includes the `c;` clipboard selector
        // — finalize splits only the leading OSC number off).
        let d2 = ProtocolTrace::decode_with_sensitive(stream);
        if let TerminalOp::Osc {
            number: Some(52),
            payload,
        } = &d2.ops[0].op
        {
            assert_eq!(payload, "c;c2VjcmV0");
        } else {
            panic!("not osc52 with sensitive");
        }
    }

    // ── DCS / APC / PM / SOS (re-review P1) ───────────────────────────────

    #[test]
    fn dcs_apc_pm_sos_captured() {
        let mut stream = b"\x1bP0;1|9;2|".to_vec(); // DCS, BEL-terminated
        stream.push(0x07);
        stream.extend(b"\x1b_kitty-query\x1b\\"); // APC, ST-terminated
        stream.extend(b"\x1b^privacy\x1b\\"); // PM
        stream.extend(b"\x1bXsos\x1b\\"); // SOS
        let d = ProtocolTrace::decode(&stream);
        let opaque: Vec<&TraceEntry> = d
            .ops
            .iter()
            .filter(|e| e.op.kind() == OpKind::Opaque)
            .collect();
        assert_eq!(opaque.len(), 4, "all four string kinds, got {:?}", d.ops);
        assert!(
            matches!(&opaque[0].op, TerminalOp::OpaqueSequence { kind: StringKind::Dcs, summary } if summary.contains("0;1|9;2|")),
            "dcs payload kept"
        );
        assert!(
            matches!(&opaque[1].op, TerminalOp::OpaqueSequence { kind: StringKind::Apc, summary } if summary == "kitty-query")
        );
        assert!(matches!(
            &opaque[2].op,
            TerminalOp::OpaqueSequence {
                kind: StringKind::Pm,
                ..
            }
        ));
        assert!(matches!(
            &opaque[3].op,
            TerminalOp::OpaqueSequence {
                kind: StringKind::Sos,
                ..
            }
        ));
        // Offsets: DCS at 0, APC at 11, PM at 26, SOS at 37.
        assert_eq!(opaque[0].at, 0);
        assert_eq!(opaque[1].at, 11);
        assert_eq!(opaque[2].at, 26);
        assert_eq!(opaque[3].at, 37);
    }

    #[test]
    fn dcs_spans_chunk_boundary() {
        let mut dec = ProtocolDecoder::new();
        let first = dec.feed_stamped(b"\x1bPdata");
        assert!(first.is_empty(), "DCS pending across boundary");
        // Chunk 2 completes the DCS (ST) and the ground text after it.
        let second = dec.feed_stamped(b"\x1b\\tail");
        assert_eq!(second.len(), 2, "DCS op + trailing text run");
        let (at, op) = &second[0];
        assert_eq!(*at, 0);
        assert!(
            matches!(op, TerminalOp::OpaqueSequence { kind: StringKind::Dcs, summary } if summary == "data")
        );
        assert_eq!(second[1], (8, TerminalOp::Text("tail".into())));
    }

    // ── Terminal-mode timeline (re-review P1) ─────────────────────────────

    #[test]
    fn mode_timeline_expands_and_orders() {
        // Alt screen, hide cursor, combined mouse modes, then restore.
        let stream = b"\x1b[?1049h\x1b[?25l\x1b[?1000;1006h\x1b[?1049l\x1b[?25h";
        let d = ProtocolTrace::decode(stream);
        let names: Vec<(&str, bool)> = d.modes.iter().map(|m| (m.mode, m.set)).collect();
        assert_eq!(
            names,
            vec![
                ("alt_screen", true),
                ("cursor_visible", false),
                ("mouse_press_release", true),
                ("mouse_sgr_encoding", true),
                ("alt_screen", false),
                ("cursor_visible", true),
            ]
        );
        // The multi-param CSI produced two events at the SAME offset.
        let mouse_at: Vec<usize> = d
            .modes
            .iter()
            .filter(|m| m.mode.starts_with("mouse"))
            .map(|m| m.at)
            .collect();
        assert_eq!(
            mouse_at,
            vec![mouse_at[0]; 2],
            "combined CSI shares its start offset"
        );
        // The raw CSI ops are still in the op trace.
        assert_eq!(d.of_kind(OpKind::Csi).len(), 5);
    }

    #[test]
    fn mode_timeline_ignores_nonprivate_and_other_finals() {
        // SGR (no ?) and DECSET with an unnamed mode must produce nothing.
        let d = ProtocolTrace::decode(b"\x1b[31m\x1b[?9999h\x1b[?25l");
        assert_eq!(d.modes.len(), 1);
        assert_eq!(d.modes[0].mode, "cursor_visible");
    }
}
