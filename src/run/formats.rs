//! Persisted-format version tags (finding 31).
//!
//! Before this module only the run manifest carried a schema tag; every
//! other persisted artifact — the transaction ledger, frame log, event
//! logs, findings, coverage ledger, focus graphs, state graph, scenarios —
//! was an unversioned JSON/JSONL blob. A reader that predates a shape
//! change failed with a bare "corrupt" warning that named nothing about
//! WHY the bytes no longer parse.
//!
//! The scheme:
//!
//! * **JSON artifacts** (single documents: findings, coverage, focus
//!   graphs, state graph, scenarios) are written as an
//!   [`Envelope`] — `{"schema": "<tag>", "payload": <the old bytes>}`.
//!   Payloads stay byte-compatible with the pre-envelope shapes, so the
//!   read path accepts BOTH: an envelope names and checks its version; a
//!   bare document (pre-envelope, or a shape that never needed one)
//!   restores as before with a warning-free best-effort parse.
//! * **JSONL streams** (transactions, frames, events) gain a
//!   self-describing first line — the [`StreamHeader`] — carrying the
//!   format tag. Header-less streams (pre-header files) restore as
//!   before; a present header MUST match, and a mismatch is a restore
//!   warning, not silent best-effort line-by-line parsing.
//!
//! Every tag is `tui-lab.<what>.v<n>`; bumping `n` is the contract that
//! the payload shape changed and old readers must refuse.

use serde::{Deserialize, Serialize};

/// The current version of every tagged format. One row per format; a
/// shape change bumps ITS row's version and updates the read path to
/// migrate (or refuse) old payloads honestly.
pub mod tags {
    /// Transaction ledger rows (`transactions.jsonl`).
    pub const LEDGER: &str = "tui-lab.ledger.v1";
    /// Frame log lines (`frames.jsonl`).
    pub const FRAMES: &str = "tui-lab.frames.v1";
    /// Terminal event logs (`events/<session>.jsonl`).
    pub const EVENTS: &str = "tui-lab.events.v1";
    /// Findings array (`findings.json`).
    pub const FINDINGS: &str = "tui-lab.findings.v1";
    /// Native coverage ledger (`coverage.json`).
    pub const COVERAGE: &str = "tui-lab.coverage.v1";
    /// Legacy label-keyed focus transitions (`focus_graph.json`).
    pub const FOCUS_LEGACY: &str = "tui-lab.focus-legacy.v1";
    /// ID-keyed focus graph (`focus_graph_ids.json`).
    pub const FOCUS_IDS: &str = "tui-lab.focus-ids.v1";
    /// State-graph export snapshot (`state_graph.json`).
    pub const STATE_GRAPH: &str = "tui-lab.state-graph.v1";
    /// Saved scenarios (`scenarios/<name>-<id>.json`).
    pub const SCENARIO: &str = "tui-lab.scenario.v1";
}

/// The versioned wrapper for single-document JSON artifacts. The payload
/// is exactly the shape that was written bare before the envelope
/// existed, so `payload` alone round-trips the historical bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Which format + version the payload is (`tags::*`).
    pub schema: String,
    /// The artifact itself — the pre-envelope document.
    pub payload: serde_json::Value,
}

impl Envelope {
    /// Wrap a payload under `tag`.
    pub fn wrap(tag: &str, payload: serde_json::Value) -> Self {
        Envelope {
            schema: tag.to_string(),
            payload,
        }
    }

    /// Serialize to pretty bytes.
    pub fn to_vec_pretty(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Parse bytes that may be an ENVELOPE (checked against `tag`) or a
    /// BARE pre-envelope document (returned as-is — the caller parses the
    /// historical shape from it). Returns an error naming the actual tag
    /// when an envelope declares a different format or version: a
    /// mismatched envelope is a real incompatibility, not corrupt bytes.
    pub fn unwrap(bytes: &[u8], tag: &str) -> anyhow::Result<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_slice(bytes)?;
        match v.get("schema").and_then(|s| s.as_str()) {
            Some(s) if s == tag => Ok(v["payload"].clone()),
            Some(other) => Err(anyhow::anyhow!(
                "format mismatch: artifact declares '{other}', this reader speaks '{tag}'"
            )),
            // Bare historical document (or an envelope for a format whose
            // payload legitimately carries a `schema` key — the contract
            // format). Caller parses best-effort.
            None => Ok(v),
        }
    }
}

/// The self-describing first line of a JSONL stream. Written once per
/// file; readers see it and either match versions or refuse with a
/// version-named warning. `written_at` is unix millis (diagnostics only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    /// Which stream this is (`tags::*`).
    pub schema: String,
    /// Unix millis at file creation.
    pub written_at: u64,
}

impl StreamHeader {
    /// Build the header line (no trailing newline — the writer adds it).
    pub fn line(tag: &str) -> String {
        let h = StreamHeader {
            schema: tag.to_string(),
            written_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        serde_json::to_string(&h).unwrap_or_else(|_| format!("{{\"schema\":\"{tag}\"}}"))
    }

    /// Classify the first line of a stream:
    ///
    /// * `Ok(Some(rest))` — a matching header; `rest` is the unparsed
    ///   remainder (everything after the first line).
    /// * `Ok(None)` — no header (pre-header stream); the whole body is
    ///   the data.
    /// * `Err` — a header is present but names a different format or
    ///   version; the error text names both sides.
    pub fn strip<'a>(body: &'a str, tag: &str) -> anyhow::Result<Option<&'a str>> {
        let Some((first, rest)) = body.split_once('\n') else {
            // A one-line body can still be a bare record; only treat it as
            // a header if it declares itself one.
            return if looks_like_header(body, tag) {
                Self::classify_line(body, tag).map(|_| Some(""))
            } else {
                Ok(None)
            };
        };
        if looks_like_header(first, tag) {
            Self::classify_line(first, tag).map(|_| Some(rest))
        } else {
            Ok(None)
        }
    }

    fn classify_line(line: &str, tag: &str) -> anyhow::Result<StreamHeader> {
        let h: StreamHeader = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("stream header unparseable: {e}"))?;
        if h.schema != tag {
            anyhow::bail!(
                "format mismatch: stream declares '{}', this reader speaks '{tag}'",
                h.schema
            );
        }
        Ok(h)
    }
}

/// Does this line look like a StreamHeader for `tag` (or any header)?
fn looks_like_header(line: &str, tag: &str) -> bool {
    line.contains("\"schema\"")
        && (line.contains(tag) || line.contains("tui-lab."))
        && !line.contains("\"seq\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_and_mismatch() {
        let payload = serde_json::json!({"k": [1, 2, 3]});
        let bytes = Envelope::wrap(tags::FINDINGS, payload.clone())
            .to_vec_pretty()
            .expect("serialize");
        let out = Envelope::unwrap(&bytes, tags::FINDINGS).expect("unwrap");
        assert_eq!(out, payload);
        // A different-version reader refuses, naming both tags.
        let err = Envelope::unwrap(&bytes, tags::COVERAGE).expect_err("mismatch");
        assert!(err.to_string().contains(tags::FINDINGS));
        assert!(err.to_string().contains(tags::COVERAGE));
        // A bare pre-envelope document passes through untouched.
        let bare = serde_json::to_vec(&payload).expect("bare");
        let out = Envelope::unwrap(&bare, tags::FINDINGS).expect("bare passthrough");
        assert_eq!(out, payload);
    }

    #[test]
    fn stream_header_roundtrip_and_bare_streams() {
        let mut body = String::new();
        body.push_str(&StreamHeader::line(tags::LEDGER));
        body.push('\n');
        body.push_str("{\"seq\":0}\n");
        let rest = StreamHeader::strip(&body, tags::LEDGER)
            .expect("matching header")
            .expect("rest after header");
        assert!(rest.contains("\"seq\":0"));
        // Mismatched reader refuses with both names.
        let err = StreamHeader::strip(&body, tags::FRAMES).expect_err("mismatch");
        assert!(err.to_string().contains(tags::LEDGER));
        // A pre-header stream (bare records) restores as before.
        let bare = "{\"seq\":0}\n{\"seq\":1}\n";
        assert!(StreamHeader::strip(bare, tags::LEDGER)
            .expect("bare stream")
            .is_none());
        // A one-line bare record is not mistaken for a header.
        assert!(StreamHeader::strip("{\"seq\":0}", tags::LEDGER)
            .expect("single record")
            .is_none());
    }
}
