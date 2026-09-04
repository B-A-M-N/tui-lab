//! The interaction transaction: the settled record of ONE act, its causal
//! render evidence, and the RAII guard bounding the mutation window.
//!
//! Split from the former single-file `execution` (review §15 god-object
//! residue).

use crate::backend::{CanonicalFrame, CaptureOutcome};
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
    // Rows with any changed cell text (the screen-level half of the causal
    // pair — same derivation as the event queue's dirty_rows).
    let dirty_rows: Vec<u16> = (0..after_state.rows)
        .filter(|&y| {
            let b = before_state
                .viewport_text
                .get(y as usize)
                .map(String::as_str);
            let a = after_state
                .viewport_text
                .get(y as usize)
                .map(String::as_str);
            b != a
        })
        .collect();
    let (window_start, window_end) = session.raw_window_range();
    if window_end == 0 {
        // Engine retains no raw bytes: no protocol evidence, honestly none.
        return None;
    }
    let (bytes, _cap, _dropped) = session.raw_output_window();
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
                .filter(|e| {
                    matches!(
                        &e.op,
                        crate::protocol::TerminalOp::Csi {
                            final_byte: 'J',
                            ..
                        }
                    )
                })
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
        dirty_cells: before_state
            .viewport_text
            .iter()
            .zip(after_state.viewport_text.iter())
            .filter(|(b, a)| b != a)
            .map(|(b, a)| b.chars().zip(a.chars()).filter(|(x, y)| x != y).count())
            .sum::<usize>()
            + before_state
                .viewport_text
                .len()
                .abs_diff(after_state.viewport_text.len())
                * after_state.cols as usize,
        dirty_rows,
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
