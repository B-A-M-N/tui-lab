//! The interaction transaction: the settled record of ONE act, its causal
//! render evidence, and the RAII guard bounding the mutation window.
//!
//! Split from the former single-file `execution` (review §15 god-object
//! residue).

use crate::backend::{CanonicalFrame, CaptureOutcome};
use crate::capture::CaptureSequenceOutcome;
use crate::screen::diff::Transition;
use crate::screen::ScreenState;
use crate::session::state::Session;

use super::record::{CanonicalAction, InputVisibility, ObservationAnchor, SettleStatus};

/// One settled interaction (Wave B item 10): canonical children hold the
/// truth, accessors derive the rest. The old shape carried the same facts
/// four ways (`action` string + `canonical`, `settled` bool + `settle_reason`,
/// `after` frame + `capture`) which could silently disagree; now:
///
/// - the action lives in [`Self::action`] (`ActionEnvelope`-level:
///   `CanonicalAction` + visibility);
/// - settlement lives in [`Self::settle`] only — `settled()` and the reason
///   derive from it;
/// - frames live in [`Self::before_frame`] / [`Self::after_frame`] as
///   [`CanonicalFrame`]s (citable `frame:N` identity), the raw
///   [`ScreenState`] reachable through `.state`;
/// - the capture outcome is the single record of *why* the after-frame is
///   authoritative (matching-frame capture), never optional for a settled
///   act;
/// - the causal render evidence lives in [`Self::render`] (re-review item
///   19): the exact protocol byte range the action produced, decoded.
#[derive(Debug, Clone)]
pub struct InteractionTransaction {
    /// The action envelope: canonical action + visibility policy.
    pub action: ActionEnvelope,
    /// The anchor that scoped the settle wait (captured before send).
    pub anchor: ObservationAnchor,
    /// The frame immediately before the input.
    pub before_frame: CanonicalFrame,
    /// The frame that satisfied the settle wait — the authoritative after
    /// (re-review P0 "matching frame"). For `SettleStatus::Skipped` this is
    /// a fresh post-action capture, clearly not a settled one.
    pub after_frame: CanonicalFrame,
    /// How the settle wait resolved.
    pub settle: SettleStatus,
    /// The before→after transition (screen + semantic diff).
    pub transition: Transition,
    /// The unified capture outcome for the after-frame (re-review P0).
    /// `None` only on the `no_wait` path, where no matching-frame capture
    /// happened.
    pub capture: Option<CaptureOutcome>,
    /// Focus across the action (re-review P1: the focus graph answers "what
    /// action caused focus to move?" — so the evidence comes from
    /// transactions, not from arbitrary reads). Each side is
    /// `(control_id, label)` from the fused analysis of the before/after
    /// frames; `None` when that side had no resolvable focus.
    pub focus_before: Option<(Option<String>, Option<String>)>,
    pub focus_after: Option<(Option<String>, Option<String>)>,
    /// Total act latency (send + settle) in milliseconds.
    pub elapsed_ms: u64,
    /// Phase timings (re-review item 39): how long the input send took vs
    /// how long the settle wait took, in milliseconds. `send_ms` isolates
    /// transport/harness cost; `settle_ms` isolates the app's own response
    /// time. Together they answer "was the app slow, or were we?"
    pub send_ms: u64,
    pub settle_ms: u64,
    /// Causal render evidence (re-review item 19): the action's exact
    /// protocol byte range, the operations it produced, the cells/rows those
    /// operations dirtied, and the input→first-output latency. `None` when
    /// the engine cannot retain raw bytes (the honest answer is "no protocol
    /// evidence", not an empty trace).
    pub render: Option<RenderTransaction>,
    /// Transition-capture evidence (audit P0-16 + finding 54): typed
    /// frame sequence with actual monotonic send-relative timestamps.
    /// `None` unless the caller armed a transition capture.
    pub transition_capture: Option<CaptureSequenceOutcome>,
    /// Which subsystem drove the input (audit finding 2): the typed
    /// provenance recorded in the ledger row. `None` only for
    /// transactions built before the field existed (tests constructing
    /// literals).
    pub origin: Option<super::record::DriveOrigin>,
    /// Exact write-boundary outcome. Evidence consumers can distinguish a
    /// refusal from a sent action from an unknown-partial transport error.
    pub dispatch: super::record::DispatchStatus,
    /// The typed dispatch failure (audit finding 4/5): present exactly when
    /// `dispatch != Sent`. Carries the classification and the backend's
    /// original error verbatim. `None` on success.
    pub dispatch_failure: Option<super::record::DispatchError>,
    /// Session event-queue range for the action window, in the EVENT-RING
    /// sequence domain (audit finding 9). `before` is captured at the final
    /// dispatch boundary (immediately before the transport write, after
    /// every pre-send pump); `after` after the final ingest/native fold.
    /// These are causal anchors for replay/probes — never the backend's
    /// `output_seq` counter.
    pub event_seq_before: u64,
    /// See [`Self::event_seq_before`]. `None` only for transactions built
    /// before the field existed (tests constructing literals).
    pub event_seq_after: Option<u64>,
    /// Native semantic-channel revision observed at the final dispatch
    /// boundary. Replay-sensitive consumers use this to compile a guard
    /// that catches native-only drift with no pixel change.
    pub native_revision_before: Option<u64>,
    /// When the dispatch failed, the recorded failure reason (mirrors
    /// [`Self::dispatch_failure`] for the tx-render path).
    pub dispatch_reason: Option<String>,
}

/// The causal render record of ONE action (re-review item 19): "pressing
/// Down caused this exact ED 2, followed by N cell writes, which dirtied
/// these rows". Everything here is measured between the raw-byte offsets
/// bracketing the action — never inferred from a rolling window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RenderTransaction {
    /// The action's signature (`key:Down`, `mouse:left:click:20:8`, ...).
    pub action: String,
    /// Absolute byte range in the child's output stream the action's
    /// response occupied: `[range_start, range_end)`. Both survive raw-ring
    /// head eviction, so the citation remains valid after the bytes are gone.
    pub range_start: u64,
    pub range_end: u64,
    /// Whether the response bytes were still retained when decoded (false =
    /// the ring evicted them; `ops` is then empty and the range is the only
    /// honest evidence).
    pub complete: bool,
    /// Number of decoded protocol operations in the response.
    pub op_count: usize,
    /// The operations, described one-per-entry (`at` = offset relative to
    /// `range_start`). Bounded — a firehose response is truncated with
    /// `ops_truncated`.
    pub ops: Vec<RenderOp>,
    /// Cells/rows the before→after transition dirtied (the screen-level
    /// half of the causal pair).
    pub dirty_cells: usize,
    pub dirty_rows: Vec<u16>,
    /// Separate dirtiness domains (audit finding 46): plain viewport text
    /// changed independently of style/geometry.
    pub dirty_textual: bool,
    /// Visual presentation changed (color/attributes/rendered pixels as
    /// represented by the canonical visual hash).
    pub dirty_visual: bool,
    /// Control/semantic structure changed.
    pub dirty_structural: bool,
    /// Input→first-response-byte latency in milliseconds (the app's own
    /// reaction time, not the settle time).
    pub first_byte_ms: Option<u64>,
    /// Item 26: input→first-screen-frame latency (the first ScreenChanged
    /// event after the action). `None` when the action never changed the
    /// parsed screen.
    pub first_frame_ms: Option<u64>,
    /// Item 26: input→first-semantic-change latency (the first
    /// SemanticChanged-class transition — screen diff that alters the
    /// controls/regions/focus reading). `None` when semantics never moved.
    pub first_semantic_ms: Option<u64>,
    /// Item 26: how many of the response's protocol ops were full-screen
    /// erase-class ops (`CSI ...J` / `CSI ...H` with wide coverage) vs the
    /// total op count — the repaint-style ratio. 0.0 when nothing rendered.
    pub full_repaint_ratio: f64,
    /// Total response bytes.
    pub bytes: u64,
}

/// One protocol operation inside a render transaction's response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RenderOp {
    /// Offset relative to [`RenderTransaction::range_start`].
    pub at: u64,
    /// The op's one-line description (`csi 2J`, `text "MENU"`, ...).
    pub describe: String,
}

/// Sampling cap for stored ops (the full trace stays decodable from the
/// raw window when retained; the transaction keeps a bounded view).
const RENDER_OP_SAMPLE_CAP: usize = 256;

/// Build the render transaction for a completed act by decoding the raw
/// bytes between the bracketing stream offsets. `bytes_total_after` is the
/// absolute stream end at settle time; `captured_at` is the monotonic
/// instant the action was sent (for first-byte latency). The latency trio
/// is computed by the caller (the executor owns the event window and the
/// backend's screen-change log; this builder only packages evidence).
#[allow(clippy::too_many_arguments)]
pub(super) fn build_render_transaction(
    session: &mut Session,
    action_sig: &str,
    offset_before: u64,
    offset_after: u64,
    first_byte_ms: Option<u64>,
    first_frame_ms: Option<u64>,
    first_semantic_ms: Option<u64>,
    before_state: &crate::screen::ScreenState,
    after_state: &crate::screen::ScreenState,
) -> Option<RenderTransaction> {
    // Audit finding 46: dirty metrics are cell-canonical, so style-only
    // changes and wide graphemes count by the actual terminal cell, not by
    // rendered string characters.
    let (dirty_cells, dirty_rows) = cell_dirty_metrics(before_state, after_state);
    let dirty_textual = before_state.viewport_text != after_state.viewport_text;
    let dirty_visual = before_state.visual_hash != after_state.visual_hash;
    let dirty_structural = before_state.structure_hash != after_state.structure_hash;
    let (window_start, window_end) = session.raw_window_range();
    if window_end == 0 {
        // Engine retains no raw bytes: no protocol evidence, honestly none.
        return None;
    }
    // Audit P1-44: a failed ring read yields NO protocol evidence (same
    // honest outcome as a zero-retention engine), with the failure visible
    // in stderr rather than silently decoded from empty bytes.
    let (bytes, _cap, _dropped) = match session.raw_output_window() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[transaction] raw output window read failed: {e}");
            return None;
        }
    };
    let lo = offset_before.max(window_start) as usize;
    let hi = offset_after.min(window_end) as usize;
    let complete = window_start <= offset_before && offset_after <= window_end;
    // Item 26: the repaint census — how many of the response's ops are
    // full-screen erase-class (CSI ... J) vs everything else. A ratio near
    // 1.0 with a large op_count says "repaints everything on every action"
    // (flicker-prone); near 0.0 says targeted diffs.
    let (op_count, ops, full_repaint_ratio) =
        if complete && lo < hi && hi <= bytes.len() + window_start as usize {
            let slice = &bytes[lo - window_start as usize..hi - window_start as usize];
            let trace = crate::protocol::ProtocolTrace::decode(slice);
            let op_count = trace.ops.len();
            let erases = trace
                .ops
                .iter()
                .filter_map(|e| EraseDisplayKind::classify(&e.op))
                .filter(EraseDisplayKind::is_full_repaint)
                .count();
            let ratio = if op_count > 0 {
                erases as f64 / op_count as f64
            } else {
                0.0
            };
            let ops: Vec<RenderOp> = trace
                .ops
                .iter()
                .take(RENDER_OP_SAMPLE_CAP)
                .map(|e| RenderOp {
                    at: e.at as u64,
                    describe: e.op.describe(),
                })
                .collect();
            (op_count, ops, ratio)
        } else {
            (0, Vec::new(), 0.0)
        };
    Some(RenderTransaction {
        action: action_sig.to_string(),
        range_start: offset_before,
        range_end: offset_after,
        complete,
        op_count,
        ops,
        dirty_cells,
        dirty_rows,
        dirty_textual,
        dirty_visual,
        dirty_structural,
        first_byte_ms,
        first_frame_ms,
        first_semantic_ms,
        full_repaint_ratio,
        bytes: offset_after.saturating_sub(offset_before),
    })
}

/// Action + persistence policy as one unit (leak fix): the executor always
/// receives the real action; every recorder consults `visibility`.
#[derive(Debug, Clone)]
pub struct ActionEnvelope {
    pub action: CanonicalAction,
    pub visibility: InputVisibility,
}

impl ActionEnvelope {
    pub fn new(action: CanonicalAction, visibility: InputVisibility) -> Self {
        ActionEnvelope { action, visibility }
    }

    /// The action's stable name.
    pub fn name(&self) -> &'static str {
        self.action.name()
    }
}

impl InteractionTransaction {
    /// The action name (derived — the old duplicated `action: String` field).
    pub fn name(&self) -> &'static str {
        self.action.name()
    }

    /// The typed canonical action, for replay/ledger.
    pub fn canonical(&self) -> &CanonicalAction {
        &self.action.action
    }

    /// Exact action provenance string (focus-graph `via` edge evidence).
    pub fn signature(&self) -> String {
        self.action.action.signature()
    }

    /// Legacy read of settlement as a bool. `Skipped` counts as *not*
    /// settled — "we did not test" must not pass a settled assertion.
    pub fn settled(&self) -> bool {
        self.settle == SettleStatus::Met
    }

    /// Why the settle resolved (derived): `Some` reason string for every
    /// status, mirroring the old `settle_reason` field.
    pub fn settle_reason(&self) -> String {
        match self.settle {
            SettleStatus::Met => self
                .capture
                .as_ref()
                .map(|c| format!("{:?}", c.reason))
                .unwrap_or_else(|| "met".to_string()),
            SettleStatus::TimedOut => "settle_budget_exhausted".to_string(),
            SettleStatus::Skipped => "no_wait".to_string(),
            SettleStatus::NotAttempted => self
                .dispatch_reason
                .clone()
                .unwrap_or_else(|| "dispatch_failed".to_string()),
        }
    }

    /// The screen state before the action.
    pub fn before(&self) -> &ScreenState {
        &self.before_frame.state
    }

    /// The screen state after the action (the matching frame when settled).
    pub fn after(&self) -> &ScreenState {
        &self.after_frame.state
    }

    /// Screen sequence of the after-frame, if the capture recorded one.
    pub fn after_screen_seq(&self) -> Option<u64> {
        self.capture.as_ref().map(|c| c.screen_seq)
    }
}

/// Execute one act against a session with proper causality:
///
/// 1. capture the event baseline *before* sending;
/// 2. send the input (routed through `send_unrecorded` when the visibility
///    policy is `Sensitive`/`NeverPersist`, so the cast never sees the
///    payload bytes — leak fix);
/// 3. wait for a screen stable *anchored after the baseline* (closes the
///    entry-pump race — re-review item 8);
/// 4. report the true [`SettleStatus`] — `Skipped` under `no_wait`, never a
///    fake "settled" (re-review item 9 / P1 fix 8).
///
/// `quiet_ms` is the quiet interval that defines "settled" (default 150ms).
/// `settle_budget_ms` bounds the wait (default quiet + 1000ms).
/// RAII recording-window guard for a single act (Wave G review P1 18:
/// "mutations through transactions").
///
/// An act's mutation window may suppress recording (sensitive visibility) and
/// raise an observation anchor. Both must be released on *every* exit path —
/// success, error, early return, panic — or the next call observes a leaked
/// suppression and the session's recording stays blind. The guard owns that
/// obligation: it is constructed with the recording state it must restore and
/// its `Drop` guarantees restoration even when a later step (e.g. the settle
/// wait) returns `Err`.
pub(super) struct ActTransactionGuard<'a> {
    session: &'a mut Session,
    /// Whether the constructor suppressed recording for this window.
    suppressed: bool,
}

impl<'a> ActTransactionGuard<'a> {
    pub(super) fn begin(session: &'a mut Session, suppress: bool) -> ActTransactionGuard<'a> {
        if suppress {
            session.suppress_recording();
        }
        ActTransactionGuard {
            session,
            suppressed: suppress,
        }
    }

    /// Reborrow the session for the transaction body. The reborrow lives only
    /// as long as the body's reads/writes; it is refreshed each call so `?`
    /// returns inside the body do not hold it past the guard's lifetime.
    pub(super) fn sess(&mut self) -> &mut Session {
        &mut *self.session
    }

    /// Success path: restore recording, then relinquish the session.
    pub(super) fn commit(mut self) {
        self.restore();
        // Swallow self so `Drop` doesn't double-restore.
        std::mem::forget(self);
    }

    /// Error path: restore recording so a later `?` returns a clean session.
    /// Idempotent with `Drop`, so callers may invoke it or not.
    pub(super) fn restore(&mut self) {
        if self.suppressed {
            self.suppressed = false;
            self.session.resume_recording();
        }
    }
}

/// Canonical cell-level dirtiness (audit finding 46): compares the actual
/// terminal grid by coordinate, grapheme text, style, and the effective
/// display width implied by the grapheme. Wide glyphs and style-only
/// changes are therefore represented accurately, unlike rendered-string
/// character counts.
pub fn cell_dirty_metrics(
    before: &crate::screen::ScreenState,
    after: &crate::screen::ScreenState,
) -> (usize, Vec<u16>) {
    use unicode_width::UnicodeWidthChar;
    let key = |c: &crate::screen::Cell| -> (u64, u64, String, String, u8) {
        // u64 packed style tuple: visibility-relevant attributes only. The
        // coordinates are added separately so sparse grids compare exactly.
        let style: u8 = u8::from(c.bold)
            | (u8::from(c.dim) << 1)
            | (u8::from(c.italic) << 2)
            | (u8::from(c.underline) << 3)
            | (u8::from(c.reverse) << 4)
            | (u8::from(c.strike) << 5);
        let width = c
            .text
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(0) as u64;
        (
            c.x as u64,
            c.y as u64,
            c.text.clone(),
            width.to_string(),
            style,
        )
    };
    let mut count: usize = 0;
    let mut rows: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    for (b, a) in before.cells.iter().zip(after.cells.iter()) {
        if b.x == a.x && b.y == a.y && key(b) == key(a) {
            continue;
        }
        count += 1;
        rows.insert(a.y);
    }
    // Dimension changes dirty every cell in the new viewport (a resize is
    // not expressible as per-cell edits).
    if before.cols != after.cols || before.rows != after.rows {
        let total = after.cols as usize * after.rows as usize;
        count = count.max(total);
        rows.extend(0..after.rows);
    }
    (count, rows.into_iter().collect())
}

/// Terminal erase-display classification (audit finding 46): ED0 clears the
/// cursor-forward area, ED1 clears backward, ED2 clears the whole screen,
/// and ED3 clears scrollback. ED2/ED3 are full-screen (or worse) repaints;
/// ED0/ED1 are partial by definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EraseDisplayKind {
    Ed0CursorForward,
    Ed1CursorBackward,
    Ed2FullScreen,
    Ed3Scrollback,
    Other,
}

impl EraseDisplayKind {
    /// Classify a CSI op. `None` when the op is not an erase-display.
    pub fn classify(op: &crate::protocol::TerminalOp) -> Option<Self> {
        let crate::protocol::TerminalOp::Csi {
            final_byte: 'J',
            params,
            ..
        } = op
        else {
            return None;
        };
        let n = params.first().copied().unwrap_or(2);
        Some(match n {
            0 => Self::Ed0CursorForward,
            1 => Self::Ed1CursorBackward,
            2 => Self::Ed2FullScreen,
            3 => Self::Ed3Scrollback,
            _ => Self::Other,
        })
    }

    pub fn is_full_repaint(&self) -> bool {
        matches!(self, Self::Ed2FullScreen | Self::Ed3Scrollback)
    }
}

#[cfg(test)]
mod transaction_metrics_tests {
    use super::*;
    use crate::screen::{Cell, Color, CursorState, ProcessState, ScreenState};

    fn screen() -> ScreenState {
        ScreenState {
            cols: 4,
            rows: 2,
            cursor: CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: vec![],
            viewport_text: vec!["abcd".to_string(), "efgh".to_string()],
            scrollback: vec![],
            hyperlinks: vec![],
            raw_hash: "raw".to_string(),
            visual_hash: "visual".to_string(),
            structure_hash: "structure".to_string(),
            process: ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: Some(1),
            },
        }
    }

    fn cell(x: u16, y: u16, text: &str, bold: bool) -> Cell {
        Cell {
            x,
            y,
            text: text.to_string(),
            fg: Color::unknown(),
            bg: Color::unknown(),
            bold,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strike: false,
        }
    }

    #[test]
    fn style_only_change_is_cell_dirty() {
        let mut before = screen();
        before.cells = vec![cell(0, 0, "A", false), cell(1, 0, "B", false)];
        let mut after = before.clone();
        after.visual_hash = "visual-bold".to_string();
        if let Some(c) = after.cells.get_mut(0) {
            c.bold = true;
        }
        let (cells, rows) = cell_dirty_metrics(&before, &after);
        assert_eq!(cells, 1);
        assert_eq!(rows, vec![0]);
    }

    #[test]
    fn wide_grapheme_change_counts_one_cell() {
        let mut before = screen();
        before.cells = vec![cell(0, 1, "a", false)];
        let mut after = before.clone();
        after.cells[0].text = "漢".to_string();
        let (cells, rows) = cell_dirty_metrics(&before, &after);
        assert_eq!(cells, 1);
        assert_eq!(rows, vec![1]);
    }

    #[test]
    fn resize_dirties_whole_viewport() {
        let mut before = screen();
        before.cells = vec![cell(0, 0, "A", false)];
        let mut after = before.clone();
        after.cols = 5;
        after.rows = 3;
        let (cells, rows) = cell_dirty_metrics(&before, &after);
        assert_eq!(cells, 15);
        assert_eq!(rows, vec![0, 1, 2]);
    }

    #[test]
    fn erase_classifier_distinguishes_ed_modes() {
        let op = |n: Vec<u16>| crate::protocol::TerminalOp::Csi {
            private: false,
            params: n,
            final_byte: 'J',
        };
        assert_eq!(
            EraseDisplayKind::classify(&op(vec![0])),
            Some(EraseDisplayKind::Ed0CursorForward)
        );
        assert!(!EraseDisplayKind::Ed0CursorForward.is_full_repaint());
        assert_eq!(
            EraseDisplayKind::classify(&op(vec![1])),
            Some(EraseDisplayKind::Ed1CursorBackward)
        );
        assert!(!EraseDisplayKind::Ed1CursorBackward.is_full_repaint());
        assert_eq!(
            EraseDisplayKind::classify(&op(vec![])),
            Some(EraseDisplayKind::Ed2FullScreen)
        );
        assert!(EraseDisplayKind::Ed2FullScreen.is_full_repaint());
        assert_eq!(
            EraseDisplayKind::classify(&op(vec![3])),
            Some(EraseDisplayKind::Ed3Scrollback)
        );
        assert!(EraseDisplayKind::Ed3Scrollback.is_full_repaint());
        let not_erase = crate::protocol::TerminalOp::Csi {
            private: false,
            params: vec![2],
            final_byte: 'H',
        };
        assert_eq!(EraseDisplayKind::classify(&not_erase), None);
    }
}
