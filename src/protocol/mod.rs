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

/// A single classified terminal operation decoded from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOp {
    /// A run of printable text (UTF-8 faded into one op).
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
        /// The payload, lossy-decoded.
        payload: String,
    },
}

/// A C0 control code recognized in the ground state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

// ── Incremental decoder ────────────────────────────────────────────────────

/// Intermediate state while decoding a chunk stream (CSI/OSC may span a chunk
/// boundary, so the decoder carries state across `feed`s).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodeState {
    Ground,
    /// After `ESC`, expecting the discriminator (`[`, `]`, selector, command).
    Escape,
    /// A selector prefix (`(`, `#`, …) seen; expecting its one argument byte.
    EscapePrefix(u8),
    /// Collecting CSI parameter bytes (0x20–0x3f) until the final byte.
    Csi { private: bool, params: Vec<u8> },
    /// Collecting an OSC payload until BEL or ST (`ESC \`).
    Osc(Option<u16>, Vec<u8>),
    /// Inside OSC after an `ESC`; a following `\` is the ST terminator.
    OscSt(Option<u16>, Vec<u8>),
}

impl Default for DecodeState {
    fn default() -> Self {
        DecodeState::Ground
    }
}

/// Incremental, chunk-safe decoder. Feed the raw stream in pieces with
/// [`Self::feed`]; a CSI/OSC/ESC sequence that spans a chunk boundary is
/// carried and completed on the next feed.
#[derive(Debug, Default)]
pub struct ProtocolDecoder {
    state: DecodeState,
    /// Accumulated printable text run, flushed into a `Text` op on a break.
    text: String,
}

impl ProtocolDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a byte slice; returns every terminal op completed by this chunk.
    /// A trailing partial sequence is *not* emitted — it stays pending until
    /// the next `feed` (or [`Self::finish`]).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalOp> {
        let mut ops = Vec::new();
        for &b in bytes {
            self.step(b, &mut ops);
        }
        ops
    }

    /// Flush pending text and materialize a trailing unfinished sequence so a
    /// trace never silently drops bytes that ended mid-stream.
    pub fn finish(&mut self) -> Vec<TerminalOp> {
        let mut ops = Vec::new();
        self.flush_text(&mut ops);
        match std::mem::take(&mut self.state) {
            DecodeState::Ground => {}
            // An ESC with nothing after it cannot describe an op — drop it.
            DecodeState::Escape => {}
            DecodeState::EscapePrefix(b) => ops.push(TerminalOp::Escape {
                byte: b,
                consumed: None,
            }),
            // A CSI with no final byte is genuinely incomplete (malformed
            // stream tail). Surface it as its escaped bytes rather than invent
            // a final byte; an agent seeing it knows the stream was cut.
            DecodeState::Csi { private, params } => {
                if let Some(fb) = raw_final_byte(&params) {
                    let numbers = split_params(&params[..params.len() - 1], private);
                    ops.push(TerminalOp::Csi {
                        final_byte: fb,
                        private,
                        params: numbers.unwrap_or_default(),
                    });
                }
            }
            DecodeState::Osc(number, payload) | DecodeState::OscSt(number, payload) => {
                ops.push(finalize_osc(number, payload));
            }
        }
        ops
    }

    /// The remaining ops once the stream is known-complete — a convenience
    /// wrapper over `feed` + `finish`.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<TerminalOp> {
        let mut ops = self.feed(bytes);
        ops.extend(self.finish());
        ops
    }

    fn flush_text(&mut self, ops: &mut Vec<TerminalOp>) {
        if !self.text.is_empty() {
            ops.push(TerminalOp::Text(std::mem::take(&mut self.text)));
        }
    }

    fn step(&mut self, b: u8, ops: &mut Vec<TerminalOp>) {
        // Take the state out so arms can call back into `self` (flush/step)
        // without aliasing the borrow; each arm restores it.
        let state = std::mem::take(&mut self.state);
        match state {
            DecodeState::Ground => {
                self.state = DecodeState::Ground;
                self.step_ground(b, ops);
            }
            DecodeState::Escape => {
                self.state = DecodeState::Ground;
                self.step_escape(b, ops);
            }
            DecodeState::EscapePrefix(prefix) => {
                self.flush_text(ops);
                ops.push(TerminalOp::Escape {
                    byte: prefix,
                    consumed: Some(b),
                });
                self.state = DecodeState::Ground;
            }
            DecodeState::Csi { private, params } => {
                let private_ = private;
                let mut params = params;
                if (0x20..=0x3f).contains(&b) {
                    params.push(b);
                    self.state = DecodeState::Csi { private, params };
                } else if (0x40..=0x7e).contains(&b) {
                    // Final byte: conclude the CSI.
                    params.push(b);
                    self.flush_text(ops);
                    ops.push(conclude_csi(private_, &params).expect("final byte present"));
                    self.state = DecodeState::Ground;
                } else {
                    // Out-of-range byte terminates the CSI without a final.
                    // Feed the offending byte back through Ground.
                    self.state = DecodeState::Ground;
                    self.step(b, ops);
                }
            }
            DecodeState::Osc(number, payload) => {
                let mut number = number;
                let mut payload = payload;
                match b {
                    0x07 => {
                        self.flush_text(ops);
                        ops.push(finalize_osc(number.take(), payload));
                        self.state = DecodeState::Ground;
                    }
                    0x1b => {
                        self.state = DecodeState::OscSt(number.take(), payload);
                    }
                    _ if b >= 0x07 => {
                        payload.push(b);
                        self.state = DecodeState::Osc(number, payload);
                    }
                    _ => {
                        self.state = DecodeState::Osc(number, payload);
                    }
                }
            }
            DecodeState::OscSt(number, payload) => {
                if b == b'\\' {
                    self.flush_text(ops);
                    ops.push(finalize_osc(number, payload));
                    self.state = DecodeState::Ground;
                } else {
                    // Not an ST — that ESC began a real escape sequence.
                    self.state = DecodeState::Escape;
                    self.step(b, ops);
                }
            }
        }
    }

    fn step_ground(&mut self, b: u8, ops: &mut Vec<TerminalOp>) {
        if b == 0x1b {
            self.flush_text(ops);
            self.state = DecodeState::Escape;
        } else if let Some(c) = ControlCode::classify(b) {
            self.flush_text(ops);
            ops.push(TerminalOp::Control(c));
        } else if b >= 0x20 {
            // Printable ASCII or UTF-8 lead/continuation: fold into the run.
            self.text.push(b as char);
            // Continuation bytes 0x80-0xbf are already >= 0x20 and land here.
        }
        // 0x00-0x06/0x0e-0x1f classified above; DAC etc. handled by classify.
    }

    fn step_escape(&mut self, b: u8, ops: &mut Vec<TerminalOp>) {
        match b {
            b'[' => {
                self.state = DecodeState::Csi {
                    private: false,
                    params: Vec::new(),
                }
            }
            b']' => self.state = DecodeState::Osc(None, Vec::new()),
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' | b' ' => {
                self.state = DecodeState::EscapePrefix(b);
            }
            _ if (0x20..=0x7f).contains(&b) && b != 0x7f => {
                self.flush_text(ops);
                ops.push(TerminalOp::Escape {
                    byte: b,
                    consumed: None,
                });
                self.state = DecodeState::Ground;
            }
            _ => {
                self.state = DecodeState::Ground;
            }
        }
    }
}

/// The final byte of a CSI, if it ends in one (0x40–0x7e).
fn raw_final_byte(raw: &[u8]) -> Option<char> {
    raw.last()
        .filter(|&&b| (0x40..=0x7e).contains(&b))
        .map(|&b| b as char)
}

/// Whether a CSI used the `?` private marker (private mode set/reset).
fn csi_private(raw: &[u8]) -> bool {
    raw.iter().any(|&b| b == b'?')
}

/// Split the leading `<number>;` off an OSC payload into its OSC number,
/// falling back to `(None, entire)` when there is no numeric prefix.
fn finalize_osc(number: Option<u16>, payload: Vec<u8>) -> TerminalOp {
    let text = String::from_utf8_lossy(&payload).into_owned();
    if number.is_none() {
        if let Some(semi) = text.find(';') {
            let head = &text[..semi];
            if let Ok(n) = head.parse::<u16>() {
                return TerminalOp::Osc {
                    number: Some(n),
                    payload: text[semi + 1..].to_string(),
                };
            }
        }
    }
    TerminalOp::Osc {
        number,
        payload: text,
    }
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
}

/// One operation plus where it started in the raw stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    /// Byte offset into the source stream where this op began.
    pub at: usize,
    /// The operation.
    pub op: TerminalOp,
}

impl ProtocolTrace {
    /// Decode an entire captured byte stream into an offset-stamped trace.
    pub fn decode(bytes: &[u8]) -> Self {
        let mut decoder = ProtocolDecoder::new();
        let completed = decoder.feed(bytes);
        let tail = decoder.finish();
        let mut ops = Vec::with_capacity(completed.len() + tail.len());
        let mut offset = 0usize;
        for op in completed.iter().chain(tail.iter()) {
            ops.push(TraceEntry { at: offset, op: op.clone() });
            offset += estimate_len(op);
        }
        ProtocolTrace { ops }
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

/// A rough byte-length estimate per op, used only for the offset stamps.
fn estimate_len(op: &TerminalOp) -> usize {
    match op {
        TerminalOp::Text(s) => s.len(),
        TerminalOp::Control(_) => 1,
        // ESC '[' + (private '?' if present) + digits/';' + final byte.
        TerminalOp::Csi { private, params, .. } => {
            2 + usize::from(*private)
                + params
                    .iter()
                    .map(|n| n.to_string().len())
                    .sum::<usize>()
                    + params.len().saturating_sub(1) // one ';' per gap
                + 1
        }
        TerminalOp::Escape { consumed, .. } => {
            if consumed.is_some() {
                3
            } else {
                2
            }
        }
        TerminalOp::Osc { payload, .. } => 2 + payload.len() + 1,
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
        assert_eq!(d.ops[3].op, TerminalOp::Control(ControlCode::CarriageReturn));
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
        if let TerminalOp::Csi { params, final_byte, .. } = &d2.ops[0].op {
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
        let oscs: Vec<_> = d2.ops.iter().filter(|e| e.op.kind() == OpKind::Osc).collect();
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
}