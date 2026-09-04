//! Shared audit-driver helpers: evidence constructors and raw-traffic
//! decoders used across the driver families.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::audit::{EvidenceKind, EvidenceRef};
use crate::protocol::ProtocolTrace;
use crate::session::state::Session;

/// Wrap a JSON detail blob into a single generic-typed EvidenceRef so
/// driver-side Finding constructors stay one-line each. The detail keeps
/// the original structured data; the kind/target/summary give the new
/// discriminated surface that other consumers can branch on (re-review
/// Part XV).
pub(super) fn ev_other(target: &str, summary: &str, detail: serde_json::Value) -> EvidenceRef {
    EvidenceRef::point(EvidenceKind::Other, target, summary).with_detail(detail)
}

/// Convenience: same as `ev_other` but the detail starts as `{}` and the
/// caller fills it in.
pub(super) fn ev_other_empty(target: &str, summary: &str) -> EvidenceRef {
    ev_other(
        target,
        summary,
        serde_json::Value::Object(Default::default()),
    )
}

/// Check if a row has border characters that suggest incomplete borders.
pub(super) fn has_incomplete_border(row: &str) -> bool {
    if row.len() < 3 {
        return false;
    }
    let first = row.chars().next().unwrap();
    let last = row.chars().last().unwrap();
    // Border chars that suggest an open edge
    let border_chars = [
        '─', '│', '┌', '┐', '└', '┘', '╭', '╮', '╰', '╯', '═', '║', '╔', '╗', '╚', '╝',
    ];
    let first_is_border = border_chars.contains(&first);
    let last_is_border = border_chars.contains(&last);
    // Incomplete if only one side has border
    first_is_border != last_is_border
}

/// Decode the session's retained raw output, or return None when the
/// backend retains nothing (cap 0) or the read FAILED — audit P1-44: the
/// failure is logged to stderr so a driver that "found no protocol
/// traffic" because its ring read errored is diagnosable rather than
/// silently conflated with "no traffic".
pub(super) fn decode_raw(session: &mut Session) -> Option<(ProtocolTrace, usize, u64)> {
    let (bytes, cap, dropped) = match session.raw_output_window() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[audit] raw output window read failed (reported as no-source): {e}");
            return None;
        }
    };
    if cap == 0 {
        return None;
    }
    Some((ProtocolTrace::decode(&bytes), bytes.len(), dropped))
}

/// Decode a CPR answer `ESC [ row ; col R` (1-based) into `(row, col)`.
pub(super) fn decode_cpr(answer: &[u8]) -> Option<(u32, u32)> {
    let s = std::str::from_utf8(answer).ok()?;
    let rest = s.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let mut parts = rest.split(';');
    let row = parts.next()?.parse::<u32>().ok()?;
    let col = parts.next()?.parse::<u32>().ok()?;
    Some((row, col))
}
