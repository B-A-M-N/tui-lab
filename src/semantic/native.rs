//! NativeSemanticProtocol side-channel (Wave F items 58–63).
//!
//! The audit's single highest-value external idea: a screen reader *infers*
//! UI structure from pixels-in-a-grid; the application *knows* its own tree.
//! When an app can cooperate, inference should defer to knowledge. This
//! module defines the cooperation contract — a file-backed NDJSON side
//! channel the harness offers to every session and cooperative apps write
//! their real semantic tree into.
//!
//! Protocol (one JSON object per line, UTF-8, `\n`-terminated):
//!
//! ```json
//! {"v":1, "type":"snapshot", "app":"my-tui", "framework":"ratatui", "nodes":[
//!   {"id":"#main", "role":"panel", "label":"Main", "bounds":[0,0,80,24],
//!    "actions":["activate"], "focusable":true, "focused":false, "value":null,
//!    "state":{"enabled":true,"checked":null}, "children":[...]}]}
//! {"v":1, "type":"event", "event":"focus", "target":"#save"}
//! ```
//!
//! Envelope rules:
//! - The harness OWNS the file: it creates a unique path per session and
//!   injects `TUI_LAB_SEMANTIC=<path>` into the child's env. Apps that
//!   never look at the variable are completely unaffected.
//! - Frames are *observations from the app*, not commands. The harness
//!   never *originates* content in the channel; its only write is
//!   compaction (audit finding 7) — once consumed history passes
//!   [`COMPACT_THRESHOLD_BYTES`], the file is rewritten to the unconsumed
//!   tail via atomic rename so disk stays bounded for a long session.
//!   Apps append per frame, so their next write lands on the new file
//!   untouched.
//! - Frames are byte-capped ([`MAX_FRAME_BYTES`]): an unterminated line
//!   wider than the cap is refused (counted in `frames_invalid`) and
//!   skipped at the next newline — a runaway writer cannot grow the
//!   reader's memory.
//! - Malformed frames are skipped and counted (`frames_invalid`) — a buggy
//!   adapter degrades to inference, never breaks observation.
//! - Native data is merged over inference with `source: "native"` and
//!   `confidence: 1.0`; matching is by role/label/bounds containment. A
//!   native node that matches nothing is still reported (in a separate
//!   `native_only` list) — never silently dropped.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::PathBuf;

/// One native event: a timestamped focus/activate/coverage signal from the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTuple {
    /// Wall-clock timestamp (ms since epoch) at ingest.
    pub ts: u64,
    /// Event name (e.g. "focus", "activate", "coverage").
    pub event: String,
    /// Event target (e.g. "#save", "src/lib.rs:42").
    pub target: String,
    /// Monotonically increasing sequence number assigned at ingest.
    pub seq: u64,
}

impl EventTuple {
    pub fn new(ts: u64, event: String, target: String, seq: u64) -> Self {
        Self {
            ts,
            event,
            target,
            seq,
        }
    }
}

/// Protocol version understood by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// The env var injected into session children.
pub const ENV_VAR: &str = "TUI_LAB_SEMANTIC";

/// Per-frame byte cap (audit finding 7). A read_line with no bound lets a
/// single unterminated line — a buggy writer, or an app that dumps a
/// snapshot with newlines stripped — grow memory without limit. 1 MiB is
/// orders of magnitude past any honest UI tree; a line that reaches it is
/// refused as invalid (counted in frames_invalid) and the reader resyncs
/// at the next newline.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Channel-file compaction threshold (audit finding 7). The channel is
/// append-only from the app's side; once consumed bytes pile up past this,
/// poll() compacts — truncating everything before the current offset — so
/// the file stays O(unconsumed frames), not O(total session traffic). 8
/// MiB of history is far past any replay need (events live in the bounded
/// ring; snapshots in `latest`).
pub const COMPACT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// A native node as declared by the application. Everything optional — the
/// adapter ships what it knows; the harness validates shapes, not intent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeNode {
    /// Stable app-chosen id (`#save-button`). Ids are the merge key.
    pub id: String,
    /// Role slug (`button`, `field`, `table`, `dialog`, ...). Free-form but
    /// lowercase-snake is expected.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Value carried by the widget (field text, checkbox state text, ...).
    #[serde(default)]
    pub value: Option<String>,
    /// `[x, y, w, h]` in cells; `null` when the app cannot know.
    #[serde(default)]
    pub bounds: Option<[u16; 4]>,
    /// Actionable verbs (`activate`, `focus`, `select`, `scroll_up`, ...).
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub focusable: Option<bool>,
    #[serde(default)]
    pub focused: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Where this widget lives in the app's source (re-review P1 item 27):
    /// framework adapters can name `file`, `line`, `symbol`, and a
    /// framework id — the strongest bridge from a rendered problem to a
    /// source edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::semantic::source_ref::SourceRef>,
    /// Nested children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<NativeNode>,
}

impl NativeNode {
    /// Flatten the tree (depth-first) into a list of (path, node) pairs.
    pub fn flatten(&self) -> Vec<(String, &NativeNode)> {
        let mut out = Vec::new();
        self.flatten_inner(String::new(), &mut out);
        out
    }

    fn flatten_inner<'a>(&'a self, prefix: String, out: &mut Vec<(String, &'a NativeNode)>) {
        let path = if prefix.is_empty() {
            self.id.clone()
        } else {
            format!("{prefix}/{}", self.id)
        };
        out.push((path.clone(), self));
        for c in &self.children {
            c.flatten_inner(path.clone(), out);
        }
    }
}

/// One frame from the app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeFrame {
    /// Protocol version (must be ≤ [`PROTOCOL_VERSION`]).
    pub v: u32,
    #[serde(rename = "type")]
    pub frame_type: String,
    /// snapshot frames: the root node (children nested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<NativeNode>,
    /// App-declared framework (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    /// App-declared name (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// event frames: what happened (`focus`, `activate`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Parse+validate one NDJSON line.
pub fn parse_frame(line: &str) -> Result<NativeFrame, String> {
    let f: NativeFrame = serde_json::from_str(line).map_err(|e| format!("invalid frame: {e}"))?;
    if f.v > PROTOCOL_VERSION {
        return Err(format!(
            "frame version {} newer than supported {}",
            f.v, PROTOCOL_VERSION
        ));
    }
    if f.frame_type != "snapshot" && f.frame_type != "event" {
        return Err(format!("unknown frame type '{}'", f.frame_type));
    }
    if f.frame_type == "snapshot" && f.root.is_none() {
        return Err("snapshot frame requires 'root'".to_string());
    }
    Ok(f)
}

/// Per-session native state: the channel path, the latest snapshot, event
/// log, and parse-health counters.
#[derive(Debug, Default)]
pub struct NativeChannel {
    pub path: Option<PathBuf>,
    pub latest: Option<NativeNode>,
    pub framework: Option<String>,
    pub app: Option<String>,
    /// Focus/activate/coverage events in a bounded ring.
    pub events: VecDeque<EventTuple>,
    /// Monotonically increasing sequence counter (assigned at ingest).
    native_seq: u64,
    pub frames_accepted: u64,
    pub frames_invalid: u64,
    /// Byte offset into the channel file already consumed.
    consumed_to: u64,
    /// Open reader (kept across polls to consume incrementally).
    reader: Option<BufReader<std::fs::File>>,
}

impl NativeChannel {
    /// Create a channel for a new session: unique path under the system
    /// temp dir. The file is created eagerly so the app can open it
    /// whenever it starts.
    pub fn create() -> std::io::Result<Self> {
        let name = format!(
            "tui-lab-semantic-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::File::create(&path)?;
        Ok(NativeChannel {
            path: Some(path),
            ..NativeChannel::default()
        })
    }

    /// The env injection for this channel (`(TUI_LAB_SEMANTIC, path)`).
    pub fn env_pair(&self) -> Option<(String, String)> {
        self.path
            .as_ref()
            .map(|p| (ENV_VAR.to_string(), p.to_string_lossy().to_string()))
    }

    /// Drain any new frames from the channel file. Bounded work per call:
    /// partial trailing lines stay buffered until complete. A file that
    /// shrank below the consumed offset (adapter restart / truncation)
    /// resets the read position.
    pub fn poll(&mut self) {
        let Some(path) = &self.path else { return };
        // Detect truncation: the file is shorter than what we consumed.
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() < self.consumed_to {
                self.consumed_to = 0;
                self.reader = None;
            }
        }
        // (Re)open the reader at the consumed offset.
        if self.reader.is_none() {
            let Ok(f) = std::fs::File::open(path) else {
                return;
            };
            let mut f = f;
            if f.seek(std::io::SeekFrom::Start(self.consumed_to)).is_err() {
                return;
            }
            self.reader = Some(BufReader::new(f));
        }
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        loop {
            let mut raw: Vec<u8> = Vec::new();
            let before_read = self.consumed_to;
            // Finding 7: bounded read. take() caps how much a single read
            // may buffer, so a runaway line cannot grow memory without
            // limit; the newline check below still distinguishes a complete
            // frame from a partial one. (read_until on bytes rather than
            // read_line: a non-UTF-8 frame must be *refused and skipped*,
            // not left to error the same offset forever.)
            match reader
                .by_ref()
                .take(MAX_FRAME_BYTES as u64)
                .read_until(b'\n', &mut raw)
            {
                Ok(0) => break, // EOF; keep reader for next poll
                Ok(_) => {
                    // A frame is only complete when newline-terminated; a
                    // partial trailing line stays unread until the app
                    // finishes writing it. Roll the offset back to the
                    // start of that line and drop the reader — its buffer
                    // may hold bytes the app rewrites, so the next poll
                    // must reopen at the rolled-back offset.
                    if !raw.ends_with(b"\n") {
                        if raw.len() >= MAX_FRAME_BYTES {
                            // Finding 7: the line filled the whole take()
                            // window with no newline — beyond any honest
                            // frame. Advance PAST the bytes read (each
                            // window counts one invalid frame) and resync
                            // at the first newline after the garbage; the
                            // next poll continues the skip in bounded
                            // steps instead of re-reading the same
                            // megabyte forever.
                            self.frames_invalid += 1;
                            self.consumed_to = before_read + raw.len() as u64;
                        } else {
                            self.consumed_to = before_read;
                        }
                        self.reader = None;
                        break;
                    }
                    self.consumed_to += raw.len() as u64;
                    // Protocol says UTF-8: a non-UTF-8 line is an invalid
                    // frame like any other — count it and move on, keeping
                    // the channel live for the frames behind it.
                    let line = match std::str::from_utf8(&raw) {
                        Ok(s) => s,
                        Err(_) => {
                            self.frames_invalid += 1;
                            continue;
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match parse_frame(trimmed) {
                        Ok(frame) => {
                            self.frames_accepted += 1;
                            match frame.frame_type.as_str() {
                                "snapshot" => {
                                    self.latest = frame.root.clone();
                                    if frame.framework.is_some() {
                                        self.framework = frame.framework.clone();
                                    }
                                    if frame.app.is_some() {
                                        self.app = frame.app.clone();
                                    }
                                }
                                "event" => {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    self.native_seq += 1;
                                    let seq = self.native_seq;
                                    self.events.push_back(EventTuple::new(
                                        now,
                                        frame.event.unwrap_or_default(),
                                        frame.target.unwrap_or_default(),
                                        seq,
                                    ));
                                    // Bounded event log (ring cap).
                                    if self.events.len() > 1024 {
                                        self.events.pop_front();
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(_) => {
                            self.frames_invalid += 1;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Finding 7: drained what there was to drain — now reclaim the
        // consumed history before it accumulates for the session's life.
        self.compact_if_needed();
    }

    /// Finding 7: keep the channel file bounded. The file is append-only
    /// from the app's side, so after a long session consumed history just
    /// piles up on disk forever. Once the file exceeds the compaction
    /// threshold, rewrite it to only the unconsumed tail: write the tail to
    /// a sibling temp file, fsync, rename over the original (atomic — a
    /// concurrent app writer either lands wholly in the old file or wholly
    /// in the new one), then reopen our reader at offset 0. Any app frame
    /// written in the race window between our tail read and the rename is
    /// dropped, which is the same outcome as a dropped frame anywhere else
    /// in the pipeline: observation degrades, the session does not.
    fn compact_if_needed(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        // Nothing consumed → nothing to reclaim (the "tail" would be the
        // whole file, and the rewrite a pointless copy).
        if meta.len() < COMPACT_THRESHOLD_BYTES || self.consumed_to == 0 {
            return;
        }
        // Read the unconsumed tail out first (the reader, if any, may sit
        // mid-file; a fresh handle seeked to the offset is the truth).
        let Ok(mut src) = std::fs::File::open(path) else {
            return;
        };
        use std::io::Seek;
        if src
            .seek(std::io::SeekFrom::Start(self.consumed_to))
            .is_err()
        {
            return;
        }
        let mut tail = Vec::new();
        if src.read_to_end(&mut tail).is_err() {
            return;
        }
        drop(src);
        let mut tmp = path.clone();
        tmp.set_extension("compact-tmp");
        let write = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp);
        let Ok(mut out) = write else { return };
        use std::io::Write;
        if out.write_all(&tail).is_err() || out.sync_all().is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // Swap in the compacted file, then restart consumption from zero.
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        self.consumed_to = 0;
        self.reader = None;
    }

    /// Merge the latest native snapshot over an inferred tree. Returns the
    /// native overlay report (matched ids, native-only ids).
    pub fn overlay(&self, tree: &mut crate::semantic::node::SemanticTree) -> NativeOverlayReport {
        let Some(root) = &self.latest else {
            return NativeOverlayReport::default();
        };
        let flat = root.flatten();
        let mut report = NativeOverlayReport::default();
        for (_path, node) in &flat {
            report.native_ids.push(node.id.clone());
            // Scoped so the mutable borrow ends before the focus clear.
            let resolved_id = {
                let resolved = resolve_node(&mut tree.root, node);
                let resolved_id: Option<String> = resolved.as_ref().map(|n| n.id.clone());
                // Focus: the app's word outranks inference — and it is
                // exclusive (finding 17): the declared node gains focus,
                // the rest of the tree loses whatever stale flag it
                // carried.
                if node.focused == Some(true) {
                    if let Some(n) = resolved {
                        n.state.focused = true;
                        report.focus_applied = Some(node.id.clone());
                    }
                }
                resolved_id
            };
            if node.focused == Some(true) {
                clear_tree_focus_except(&mut tree.root, resolved_id.as_deref());
            }
            if resolved_id.is_none() {
                // Finding 16: unmatched may mean AMBIGUOUS — report the
                // competing nodes instead of a bare miss.
                let competing = collect_ambiguous_ids(&tree.root, node);
                if !competing.is_empty() {
                    report.ambiguous.push((node.id.clone(), competing));
                }
            }
            match resolve_node(&mut tree.root, node) {
                Some(n) => {
                    // Tree-only path: affordances live on the node; the flat
                    // mirror below does not exist in this entry point.
                    let _ = apply_native_facts(n, node);
                    report.matched.push(node.id.clone());
                }
                None => {
                    report.native_only.push(node.id.clone());
                }
            }
        }
        report
    }

    /// Fused merge (re-review Wave-4): resolve each native node ONCE and
    /// write it into both shapes — the `SemanticTree` (nodes mode) and the
    /// flat [`crate::semantic::SemanticScreen`] (semantic/summary/tree modes)
    /// — so every observe mode reports the same truth. A native node that
    /// matches a tree node derived from a `Control` also fixes up that
    /// control (control-derived tree nodes keep the control's id), and a
    /// native focus declaration rewrites `sem.focus` wholesale: the app's
    /// word on focus outranks style inference everywhere, not just in the
    /// nodes view.
    pub fn overlay_fused(
        &self,
        tree: &mut crate::semantic::node::SemanticTree,
        sem: &mut crate::semantic::SemanticScreen,
    ) -> NativeOverlayReport {
        let Some(root) = &self.latest else {
            return NativeOverlayReport::default();
        };
        let flat = root.flatten();
        let mut report = NativeOverlayReport::default();
        // Finding 17: collect the app's focus verdict FIRST, so the clear
        // below can run against every non-focused control — a stale
        // `focused: true` from inference (or an earlier frame) must not
        // survive a frame where the app says focus lives elsewhere.
        let native_focus_target: Option<String> = flat
            .iter()
            .find(|(_, n)| n.focused == Some(true))
            .map(|(_, n)| n.id.clone());
        for (_path, node) in &flat {
            report.native_ids.push(node.id.clone());
            // Scoped so the mutable borrow ends before the focus clear.
            // The join facts are captured inside — the tree node's id is
            // the key back into `sem.controls`.
            let resolved_id = {
                let resolved = resolve_node(&mut tree.root, node);
                let resolved_id: Option<String> = resolved.as_ref().map(|n| n.id.clone());
                let resolved_label: Option<String> =
                    resolved.as_ref().and_then(|n| n.label.clone());
                if node.focused == Some(true) {
                    if let Some(n) = resolved {
                        n.state.focused = true;
                        report.focus_applied = Some(node.id.clone());
                    }
                    // Fused: focus is mode-independent. Resolve the label
                    // from the tree node when the native node carries none
                    // apps often declare ids only.
                    sem.focus = crate::semantic::FocusInfo {
                        control: node.label.clone().or(resolved_label),
                        control_id: Some(resolved_id.clone().unwrap_or_else(|| node.id.clone())),
                        confidence: 1.0,
                        evidence: vec![format!("native-focus:{}", node.id)],
                    };
                }
                resolved_id
            };
            if node.focused == Some(true) {
                // Finding 17, tree side: focus is exclusive — every OTHER
                // node loses its focused flag, whatever inference or a
                // previous frame claimed.
                clear_tree_focus_except(&mut tree.root, resolved_id.as_deref());
            }
            if resolved_id.is_none() {
                // Finding 16: unmatched may mean AMBIGUOUS — report the
                // competing nodes instead of a bare miss.
                let competing = collect_ambiguous_ids(&tree.root, node);
                if !competing.is_empty() {
                    report.ambiguous.push((node.id.clone(), competing));
                }
            }
            if let Some(n) = resolve_node(&mut tree.root, node) {
                let affordances = apply_native_facts(n, node);
                // Mirror the same facts into the flat control with the same
                // id (control-derived tree nodes keep the control's id, so
                // the join key survives the tree build).
                let joined_id = resolved_id.clone();
                let focused_flag = node.focused;
                if let Some(c) = sem
                    .controls
                    .iter_mut()
                    .find(|c| Some(c.id.clone()) == joined_id)
                {
                    if let Some(focused) = focused_flag {
                        c.focused = focused;
                    }
                    // Native role refinement on the flat shape too (re-review
                    // P0): inference says Unknown + native says button ⇒ the
                    // control becomes a button.
                    if !node.role.is_empty() {
                        if let Some(kind) = kind_from_slug(&node.role) {
                            c.kind = kind;
                            c.source = "native".to_string();
                            c.confidence = crate::semantic::Confidence::native();
                        }
                    }
                    if let Some(enabled) = node.enabled {
                        c.enabled = enabled;
                        c.source = "native".to_string();
                    }
                    if let Some(focusable) = node.focusable {
                        c.focusable = focusable;
                    }
                    if let Some(value) = &node.value {
                        c.value = Some(value.clone());
                    }
                    if let Some(label) = &node.label {
                        // The control keeps its inferred label unless it had
                        // none; the tree node mirrors the same rule above.
                        if c.label.is_empty() {
                            c.label = label.clone();
                        }
                    }
                }
                // Finding 8, fused: the flat screen's affordance list
                // carries the same declared verbs the tree node does —
                // replacing ALL prior entries for this control exactly as
                // the tree side did (a refreshed frame re-declares, and
                // verbs the app no longer declares do not survive). A
                // control the app declares nothing about keeps its
                // inferred affordances untouched.
                if !node.actions.is_empty() {
                    let ctl = resolved_id.clone();
                    sem.affordances
                        .retain(|a| a.control_id.as_deref() != ctl.as_deref());
                    sem.affordances.extend(affordances);
                }
                report.matched.push(node.id.clone());
            } else {
                // Re-review P0 (native-only merge): an unmatched native node
                // with bounds is semantic truth the app volunteered — it must
                // become actionable state, not a report line. Insert it into
                // both shapes: the tree under the nearest containing node,
                // and the flat controls list as a native-sourced control.
                let inserted = insert_native_only(&mut tree.root, node);
                if inserted {
                    if let Some(c) = native_node_to_control(node) {
                        sem.controls.push(c);
                    }
                    // Finding 8: native-only nodes declare affordances too.
                    if !node.actions.is_empty() {
                        let label = node.label.clone().unwrap_or_else(|| node.id.clone());
                        for verb in &node.actions {
                            sem.affordances
                                .push(crate::semantic::affordance::Affordance {
                                    action: label.clone(),
                                    control_id: Some(node.id.clone()),
                                    invocation: verb_to_invocation(verb),
                                    visibility: crate::semantic::affordance::Visibility::Labeled,
                                    hint_text: node.label.clone(),
                                    confidence: crate::semantic::Confidence::native(),
                                    source: "native".to_string(),
                                });
                        }
                    }
                    report.native_only.push(node.id.clone());
                } else {
                    // No bounds: a pointer without geometry cannot be placed;
                    // it stays report-only, honestly.
                    report.native_only.push(node.id.clone());
                }
            }
        }
        // Finding 17, flat side: the app's focus verdict is exclusive —
        // when it names a target, every OTHER control loses its focused
        // flag. Done after the loop so it covers controls whose native
        // counterpart did not resolve: inference's style-guessed focus and
        // an earlier frame's focus must not both be live.
        if let Some(target) = &native_focus_target {
            // The control id the focus landed on (the resolved join id, or
            // the native id itself for native-only insertions).
            let focused_control = sem
                .focus
                .control_id
                .clone()
                .unwrap_or_else(|| target.clone());
            for c in sem.controls.iter_mut() {
                if c.id != focused_control {
                    c.focused = false;
                }
            }
        }
        report
    }

    /// Return events whose sequence number is strictly greater than `seq`,
    /// in chronological order. This is the ring-safe consumption API: unlike
    /// positional slicing it correctly yields new events even after the ring
    /// has wrapped around (when `Vec`-based `remove(0)` cap would leave the
    /// cursor past `len`).
    pub fn events_since(&self, seq: u64) -> Vec<EventTuple> {
        self.events
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }

    /// The highest sequence number seen so far (0 when no events have been
    /// ingested yet).
    pub fn last_native_seq(&self) -> u64 {
        self.native_seq
    }

    /// Monotonic channel revision for stale-state guards: the latest
    /// native sequence when the channel exists, otherwise `None`. A guard
    /// can therefore distinguish "no native channel" from "no native
    /// update yet" without treating zero as a live revision.
    pub fn revision(&self) -> Option<u64> {
        self.path.as_ref().map(|_| self.native_seq)
    }

    /// Reset (session restart): keep the path, drop the state.
    pub fn reset(&mut self) {
        self.latest = None;
        self.events.clear();
        self.native_seq = 0;
        self.frames_accepted = 0;
        self.frames_invalid = 0;
        self.consumed_to = 0;
        self.reader = None;
        // Truncate the channel file so the new generation starts clean.
        if let Some(p) = &self.path {
            let _ = std::fs::write(p, b"");
        }
    }
}

/// Result of overlaying native data on an inferred tree.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct NativeOverlayReport {
    pub native_ids: Vec<String>,
    pub matched: Vec<String>,
    pub native_only: Vec<String>,
    pub focus_applied: Option<String>,
    /// Finding 16: native ids that matched MORE than one inferred node
    /// (by suffix or label) and were therefore left unmatched — ambiguity
    /// is reported, never silently resolved to the first hit. The inner
    /// vec lists the competing inferred-node ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<(String, Vec<String>)>,
}

impl NativeOverlayReport {
    pub fn active(&self) -> bool {
        !self.native_ids.is_empty()
    }
}

fn find_by_id<'a>(
    node: &'a mut crate::semantic::node::SemanticNode,
    id: &str,
) -> Option<&'a mut crate::semantic::node::SemanticNode> {
    if node.id == id {
        return Some(node);
    }
    for c in node.children.iter_mut() {
        if let Some(found) = find_by_id(c, id) {
            return Some(found);
        }
    }
    None
}

/// Finding 17: focus is exclusive. Clear `focused` on every tree node
/// EXCEPT `keep` (the node the app declared focused). A native snapshot
/// is a whole-frame verdict, not a delta — a stale `focused: true` from
/// style inference or an earlier frame must not survive beside it.
fn clear_tree_focus_except(node: &mut crate::semantic::node::SemanticNode, keep: Option<&str>) {
    if Some(node.id.as_str()) != keep {
        node.state.focused = false;
    }
    for c in node.children.iter_mut() {
        clear_tree_focus_except(c, keep);
    }
}

/// Finding 8: the app-declared action verbs become affordances. The
/// protocol's verb vocabulary is open (apps may declare `actions:
/// ["export-csv"]`), so mapping is conventional where a verb has a
/// conventional invocation and honest [`Invocation::Declared`] where it
/// does not: `activate`/`toggle`/`select`/`open`/`press` are
/// focus-and-Enter interactions on a TUI, `focus` moves focus, and
/// everything else is reported as the app stated it rather than guessed
/// into a wrong invocation shape.
fn verb_to_invocation(verb: &str) -> crate::semantic::affordance::Invocation {
    use crate::semantic::affordance::Invocation;
    match verb {
        "activate" | "toggle" | "select" | "open" | "press" => Invocation::Activate,
        "focus" => Invocation::Navigate {
            key: "tab".to_string(),
        },
        other => Invocation::Declared {
            verb: other.to_string(),
        },
    }
}

/// Finding 8: materialize one native node's declared actions as
/// affordances on the matched node. The app's declaration replaces
/// inference for that node: prior native affordances are dropped (a
/// refreshed frame re-declares), and an inferred activate-affordance the
/// app did not declare is dropped too — the app knows which verbs its
/// widget supports. Returns the newly declared affordances so the fused
/// path can mirror them into the flat screen.
fn apply_native_actions(
    n: &mut crate::semantic::node::SemanticNode,
    node: &NativeNode,
) -> Vec<crate::semantic::affordance::Affordance> {
    let declares_activate = node.actions.iter().any(|v| {
        matches!(
            verb_to_invocation(v),
            crate::semantic::affordance::Invocation::Activate
        )
    });
    n.affordances.retain(|a| {
        if a.source == "native" {
            return false; // replaced wholesale by this frame's declaration
        }
        // Inferred Activate survives only when the app declares nothing
        // (no actions → no opinion) or declares an activating verb.
        !(matches!(
            a.invocation,
            crate::semantic::affordance::Invocation::Activate
        ) && !node.actions.is_empty()
            && !declares_activate)
    });
    if node.actions.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for verb in &node.actions {
        out.push(crate::semantic::affordance::Affordance {
            action: node.label.clone().unwrap_or_else(|| node.id.clone()),
            control_id: Some(n.id.clone()),
            invocation: verb_to_invocation(verb),
            visibility: crate::semantic::affordance::Visibility::Labeled,
            hint_text: node.label.clone(),
            confidence: crate::semantic::Confidence::native(),
            source: "native".to_string(),
        });
    }
    n.affordances.extend(out.iter().cloned());
    out
}

/// Apply one native node's declared facts onto its resolved tree node.
/// Shared by [`NativeChannel::overlay`] and
/// [`NativeChannel::overlay_fused`] — the two entry points must never drift
/// apart in what they write. Returns the affordances the node's declared
/// actions materialized as (finding 8) so the fused path can mirror them
/// into the flat screen; the tree-only path has them on the node already.
fn apply_native_facts(
    n: &mut crate::semantic::node::SemanticNode,
    node: &NativeNode,
) -> Vec<crate::semantic::affordance::Affordance> {
    // Role merge (re-review P0: native role is the primary reason the
    // side-channel exists — inference says Unknown, the app says button,
    // the fused tree must say button). Native role wins over Unknown /
    // lowest-confidence inference; a non-Unknown inferred role is kept
    // unless it disagrees, in which case native still wins (the app's
    // self-report is the stronger evidence) but the node keeps its
    // confidence rather than claiming 1.0.
    if !node.role.is_empty() {
        if let Some(role) = role_from_slug(&node.role) {
            n.role = role;
            n.confidence = crate::semantic::Confidence::native();
        }
    }
    // Enable provenance: app-declared enabled is native evidence.
    if let Some(enabled) = node.enabled {
        n.state.enabled = crate::semantic::node::EnabledState {
            value: enabled,
            source: "native".to_string(),
            confidence: 1.0,
        };
    }
    if let Some(focusable) = node.focusable {
        n.state.focusable = focusable;
    }
    if let Some(label) = &node.label {
        if n.label.as_deref().map(str::is_empty).unwrap_or(true) {
            n.label = Some(label.clone());
        }
    }
    if let Some(value) = &node.value {
        n.value = Some(value.clone());
    }
    // Identity join (item 28): a matched node keeps its semantic id and
    // gains the native id + source locus, so a finding on the rendered node
    // can point at the app's code. Later frames refresh `source_refs` in
    // place — adapters can sharpen a locus over time.
    if n.identity.is_none() {
        n.identity = Some(crate::semantic::source_ref::ComponentIdentity::semantic(
            n.id.clone(),
        ));
    }
    if let Some(identity) = &mut n.identity {
        identity.native_id = Some(node.id.clone());
        if let Some(src) = &node.source {
            if !identity.source_refs.iter().any(|r| r == src) {
                identity.source_refs.push(src.clone());
            }
        }
    }
    // Finding 8: declared actions are affordances, computed last so the
    // affordances carry the final label/id facts.
    apply_native_actions(n, node)
}

/// Map a native role slug onto the semantic [`Role`] vocabulary. Unknown
/// slugs map to `None` — the node keeps its inferred role rather than
/// degrading to `Unknown`.
fn role_from_slug(slug: &str) -> Option<crate::semantic::node::Role> {
    use crate::semantic::node::Role;
    Some(match slug.to_lowercase().as_str() {
        "button" => Role::Button,
        "field" | "textbox" | "input" => Role::Field,
        "checkbox" => Role::Checkbox,
        "radio" => Role::Radio,
        "tab" => Role::Tab,
        "list" => Role::List,
        "listitem" | "list_item" | "item" => Role::ListItem,
        "menu" => Role::Menu,
        "menuitem" | "menu_item" => Role::MenuItem,
        "table" => Role::Table,
        "tree" => Role::Tree,
        "treeitem" | "tree_item" => Role::TreeItem,
        "dialog" => Role::Dialog,
        "panel" => Role::Panel,
        "toolbar" => Role::Toolbar,
        "footer" | "statusbar" | "status_bar" => Role::Footer,
        "textarea" | "text_area" => Role::TextArea,
        "select" | "dropdown" | "combobox" => Role::Dropdown,
        "progressbar" | "progress_bar" | "progress" => Role::Progress,
        "spinner" => Role::Spinner,
        "label" | "text" => Role::Label,
        "hyperlink" | "link" => Role::Hyperlink,
        "screen" | "window" => Role::Screen,
        _ => return None,
    })
}

/// The flat-shape counterpart of [`role_from_slug`]. Derives from the
/// unified Role→ControlKind projection (re-review item 36) — one
/// vocabulary, no drift between the tree shape and the flat shape.
fn kind_from_slug(slug: &str) -> Option<crate::semantic::controls::ControlKind> {
    role_from_slug(slug).and_then(|r| r.control_kind())
}

/// Test visibility for the drift check (item 36): the two slug paths must
/// agree, so the tests exercise them directly.
#[cfg(test)]
pub fn native_role_for_test(slug: &str) -> Option<crate::semantic::node::Role> {
    role_from_slug(slug)
}

/// Test visibility counterpart.
#[cfg(test)]
pub fn native_kind_for_test(slug: &str) -> Option<crate::semantic::controls::ControlKind> {
    kind_from_slug(slug)
}

/// Re-review P0 (native-only insertion): place an unmatched native node into
/// the inferred tree. Attach under the smallest containing node (root when
/// nothing contains it); returns `false` when the node has no usable bounds.
fn insert_native_only(root: &mut crate::semantic::node::SemanticNode, native: &NativeNode) -> bool {
    let [x, y, w, h] = match native.bounds {
        Some(b) if b[2] > 0 && b[3] > 0 => b,
        _ => return false,
    };
    let mut n = native_to_semantic_node(native, x, y, w, h);
    // Find the smallest node whose bounds fully contain the native bounds —
    // the native parent when the app declared a hierarchy, else the inferred
    // container.
    let bounds = n.bounds.clone();
    let target_id = smallest_containing_id(root, &bounds);
    n.parent = Some(target_id.clone().unwrap_or_else(|| root.id.clone()));
    match target_id {
        Some(id) => {
            if let Some(t) = root.find_mut(&id) {
                t.children.push(n);
            } else {
                root.children.push(n);
            }
        }
        None => root.children.push(n),
    }
    true
}

/// Build a [`SemanticNode`] from a native node with known bounds: role from
/// the slug (Unknown when unmapped), `native_only` provenance via
/// confidence 1.0 and source-tagged evidence.
fn native_to_semantic_node(
    native: &NativeNode,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
) -> crate::semantic::node::SemanticNode {
    crate::semantic::node::SemanticNode {
        id: native.id.clone(),
        role: role_from_slug(&native.role).unwrap_or(crate::semantic::node::Role::Unknown),
        parent: None,
        bounds: crate::semantic::regions::Bounds {
            x,
            y,
            width: w,
            height: h,
        },
        label: native.label.clone(),
        value: native.value.clone(),
        state: crate::semantic::node::NodeState {
            focusable: native.focusable.unwrap_or(false),
            focused: native.focused.unwrap_or(false),
            enabled: crate::semantic::node::EnabledState {
                value: native.enabled.unwrap_or(true),
                source: "native".to_string(),
                confidence: 1.0,
            },
            ..crate::semantic::node::NodeState::default()
        },
        children: Vec::new(),
        affordances: Vec::new(),
        // Native-only insertion (item 28): the identity is native from
        // birth — semantic id and native id coincide, and any source locus
        // the adapter declared joins directly.
        identity: Some(
            crate::semantic::source_ref::ComponentIdentity::semantic(native.id.clone())
                .with_native(native.id.clone())
                .with_source_refs(native.source.iter().cloned().collect()),
        ),
        confidence: crate::semantic::Confidence::native(),
    }
}

/// The flat-shape counterpart: a native-only node also becomes a Control so
/// flat-mode consumers (summary/semantic views) see the same truth.
///
/// Re-review P0.11: an unknown native role stays unknown. The old
/// `unwrap_or(ControlKind::Button)` invented an actionable control out of
/// anything the vocabulary didn't cover — a native role "graph" became a
/// clickable Button and polluted exploration, mouse auditing, intent
/// resolution, and risk classification. `ControlKind::Unknown` exists for
/// exactly this; downstream consumers already treat it as non-actionable.
fn native_node_to_control(native: &NativeNode) -> Option<crate::semantic::controls::Control> {
    let [x, y, w, h] = native.bounds?;
    Some(crate::semantic::controls::Control {
        id: native.id.clone(),
        kind: kind_from_slug(&native.role)
            .unwrap_or(crate::semantic::controls::ControlKind::Unknown),
        label: native.label.clone().unwrap_or_else(|| native.id.clone()),
        value: native.value.clone(),
        bounds: crate::semantic::controls::ControlBounds {
            x,
            y,
            width: w,
            height: h,
        },
        region_id: None,
        focusable: native.focusable.unwrap_or(false),
        focused: native.focused.unwrap_or(false),
        enabled: native.enabled.unwrap_or(true),
        selected: false,
        checked: false,
        shortcut: None,
        confidence: crate::semantic::Confidence::native(),
        evidence: vec!["native-only-insertion".to_string()],
        source: "native".to_string(),
    })
}

/// The id of the smallest node in the tree whose bounds fully contain `b`.
/// `None` when only the root would qualify (the caller attaches to root
/// children directly).
fn smallest_containing_id(
    node: &crate::semantic::node::SemanticNode,
    b: &crate::semantic::regions::Bounds,
) -> Option<String> {
    // Descend first: the deepest container wins.
    for child in &node.children {
        if let Some(found) = smallest_containing_id(child, b) {
            return Some(found);
        }
    }
    let contains = node.bounds.x <= b.x
        && node.bounds.y <= b.y
        && node.bounds.x + node.bounds.width >= b.x + b.width
        && node.bounds.y + node.bounds.height >= b.y + b.height;
    // Do not offer the root as a "container" — root.children.push is the
    // fallback in the caller.
    if contains && node.parent.is_some() {
        Some(node.id.clone())
    } else {
        None
    }
}

/// Resolve a native node to its inferred counterpart.
///
/// Exact id match first. Apps name nodes in their own vocabulary (`#save`,
/// `okButton`) while inference produces path ids (`button/save`), so exact
/// matches alone would leave most cooperative apps unmatched. Fallbacks, in
/// order: id-suffix path segment (`#save` ↔ `…/button/save`), then unique
/// label equality — a label match is only trusted when exactly one inferred
/// node carries it, so ambiguity stays unmatched rather than guessed.
fn resolve_node<'a>(
    root: &'a mut crate::semantic::node::SemanticNode,
    native: &NativeNode,
) -> Option<&'a mut crate::semantic::node::SemanticNode> {
    // Decide which *key* matches first (id / suffix / label), then borrow
    // once — returning early from inside `if let Some(n)` holds the first
    // mutable borrow across the later lookups. Finding 16: EVERY relaxed
    // key is uniqueness-checked — a suffix hit used to resolve to the
    // first matching node while a second one sat unnoticed, so the app's
    // declaration could silently land on the wrong widget.
    if find_by_key(root, &native.id, 0).is_some() {
        return find_by_id(root, &native.id);
    }
    let want = native.id.trim_start_matches(['#', '@']).to_lowercase();
    let mut suffix_hits: Vec<&crate::semantic::node::SemanticNode> = Vec::new();
    if !want.is_empty() {
        collect_by_suffix_key(root, &want, &mut suffix_hits);
    }
    if suffix_hits.len() == 1 {
        let id = suffix_hits[0].id.clone();
        return find_by_id(root, &id);
    }
    if let Some(label) = &native.label {
        let mut hits: Vec<&crate::semantic::node::SemanticNode> = Vec::new();
        collect_by_label(root, label, &mut hits);
        if hits.len() == 1 {
            let id = hits[0].id.clone();
            return find_by_id(root, &id);
        }
    }
    None
}

/// Ambiguous suffix/label resolution (finding 16): the tree nodes that
/// COMPETED for a native id, for the overlay report — an ambiguity is
/// reported, never silently resolved to the first hit.
pub(crate) fn collect_ambiguous_ids(
    root: &crate::semantic::node::SemanticNode,
    native: &NativeNode,
) -> Vec<String> {
    let mut out = Vec::new();
    if find_by_key(root, &native.id, 0).is_some() {
        return out; // exact id: unique by definition
    }
    let want = native.id.trim_start_matches(['#', '@']).to_lowercase();
    if !want.is_empty() {
        let mut hits = Vec::new();
        collect_by_suffix_key(root, &want, &mut hits);
        if hits.len() > 1 {
            out.extend(hits.iter().map(|n| n.id.clone()));
            return out;
        }
    }
    if let Some(label) = &native.label {
        let mut hits = Vec::new();
        collect_by_label(root, label, &mut hits);
        if hits.len() > 1 {
            out.extend(hits.iter().map(|n| n.id.clone()));
        }
    }
    out
}

fn find_by_key<'a>(
    node: &'a crate::semantic::node::SemanticNode,
    id: &str,
    _depth: usize,
) -> Option<&'a crate::semantic::node::SemanticNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_by_key(c, id, 0))
}

/// ALL nodes whose last path segment equals `want` (finding 16: the count,
/// not just the first hit, is what decides trust).
fn collect_by_suffix_key<'a>(
    node: &'a crate::semantic::node::SemanticNode,
    want: &str,
    out: &mut Vec<&'a crate::semantic::node::SemanticNode>,
) {
    let last = node.id.rsplit('/').next().unwrap_or("").to_lowercase();
    if last == want {
        out.push(node);
    }
    for c in &node.children {
        collect_by_suffix_key(c, want, out);
    }
}

fn collect_by_label<'a>(
    node: &'a crate::semantic::node::SemanticNode,
    label: &str,
    out: &mut Vec<&'a crate::semantic::node::SemanticNode>,
) {
    if node.label.as_deref() == Some(label) {
        out.push(node);
    }
    for c in &node.children {
        collect_by_label(c, label, out);
    }
}

/// Whether a launch env already carries a native channel (skip re-injection).
pub fn env_has_channel(env: &[(String, String)]) -> bool {
    env.iter().any(|(k, _)| k == ENV_VAR)
}

/// Test-only helpers shared with the fused-truth tests.
#[cfg(test)]
pub(crate) mod tests_support {
    /// Find the first tree node carrying `label` (depth-first).
    pub fn find_label<'a>(
        node: &'a crate::semantic::node::SemanticNode,
        label: &str,
    ) -> Option<&'a crate::semantic::node::SemanticNode> {
        if node.label.as_deref() == Some(label) {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_label(c, label))
    }

    /// Count leaf nodes with interactive roles (the tree-side counterpart of
    /// `SemanticScreen::controls` for the same detection pass).
    pub fn count_controls(node: &crate::semantic::node::SemanticNode) -> usize {
        let self_count = match node.role {
            crate::semantic::node::Role::Button
            | crate::semantic::node::Role::Field
            | crate::semantic::node::Role::Checkbox
            | crate::semantic::node::Role::Tab
            | crate::semantic::node::Role::ListItem => 1,
            _ => 0,
        };
        self_count + node.children.iter().map(count_controls).sum::<usize>()
    }
}

/// Session-level registry of native channels (session id → channel).
#[derive(Default)]
pub struct ChannelRegistry {
    channels: HashMap<String, NativeChannel>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create + register a channel for a session. Returns the env pair.
    pub fn attach(&mut self, session_id: &str) -> Option<(String, String)> {
        match NativeChannel::create() {
            Ok(ch) => {
                let pair = ch.env_pair();
                self.channels.insert(session_id.to_string(), ch);
                pair
            }
            Err(_) => None,
        }
    }

    pub fn get(&self, session_id: &str) -> Option<&NativeChannel> {
        self.channels.get(session_id)
    }

    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut NativeChannel> {
        self.channels.get_mut(session_id)
    }

    pub fn remove(&mut self, session_id: &str) {
        if let Some(ch) = self.channels.remove(session_id) {
            if let Some(p) = &ch.path {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Poll every channel (called before observations).
    pub fn poll_all(&mut self) {
        for ch in self.channels.values_mut() {
            ch.poll();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_frame_parses() {
        let line = r##"{"v":1,"type":"snapshot","app":"x","framework":"ratatui","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save","bounds":[2,3,6,1],"actions":["activate"],"focusable":true,"focused":true}]}}"##;
        let f = parse_frame(line).expect("parse");
        assert_eq!(f.frame_type, "snapshot");
        let root = f.root.expect("root");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].id, "#save");
        assert_eq!(root.children[0].focused, Some(true));
    }

    #[test]
    fn malformed_frames_are_rejected_not_panicking() {
        assert!(parse_frame("{not json").is_err());
        assert!(parse_frame(r#"{"v":99,"type":"snapshot","root":null}"#).is_err());
        assert!(parse_frame(r#"{"v":1,"type":"teleport"}"#).is_err());
        assert!(parse_frame(r#"{"v":1,"type":"snapshot"}"#).is_err());
    }

    #[test]
    fn flatten_covers_depth() {
        let root = NativeNode {
            id: "#a".into(),
            role: "screen".into(),
            label: None,
            value: None,
            bounds: None,
            actions: vec![],
            focusable: None,
            source: None,
            focused: None,
            enabled: None,
            children: vec![NativeNode {
                id: "#b".into(),
                role: "panel".into(),
                label: None,
                value: None,
                bounds: None,
                actions: vec![],
                focusable: None,
                source: None,
                focused: None,
                enabled: None,
                children: vec![NativeNode {
                    id: "#c".into(),
                    role: "button".into(),
                    label: Some("C".into()),
                    value: None,
                    bounds: None,
                    actions: vec!["activate".into()],
                    focusable: Some(true),
                    source: None,
                    focused: None,
                    enabled: Some(false),
                    children: vec![],
                }],
            }],
        };
        let flat = root.flatten();
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[2].0, "#a/#b/#c");
        assert_eq!(flat[2].1.enabled, Some(false));
    }

    #[test]
    fn channel_round_trip_file() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                r##"{"v":1,"type":"snapshot","root":{"id":"#r","role":"screen"}}"##
            ),
        )
        .expect("write");
        ch.poll();
        assert_eq!(ch.frames_accepted, 1);
        assert!(ch.latest.is_some());
        assert_eq!(ch.latest.as_ref().expect("latest").id, "#r");
        // Cleanup.
        std::fs::remove_file(&path).ok();
    }

    /// A partial trailing line is not consumed until complete.
    #[test]
    fn partial_line_waits_for_completion() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        std::fs::write(&path, r#"{"v":1,"type":"snapshot""#).expect("write partial");
        ch.poll();
        assert_eq!(ch.frames_accepted, 0, "partial line must not parse");
        assert_eq!(ch.frames_invalid, 0);
        // Complete it.
        std::fs::write(
            &path,
            format!(
                "{}\n",
                r##"{"v":1,"type":"snapshot","root":{"id":"#r","role":"screen"}}"##
            ),
        )
        .expect("write full");
        ch.poll();
        assert_eq!(ch.frames_accepted, 1);
        std::fs::remove_file(&path).ok();
    }

    // ── Finding 8: NativeNode.actions are consumed, not dead data ──

    /// A tiny two-button screen, the same shape the fused-truth tests use.
    fn two_button_screen() -> crate::screen::ScreenState {
        use crate::screen::cell::{Cell, Color, ProcessState};
        let mut cells = Vec::new();
        let mut viewport_text = Vec::new();
        for (y, line) in ["[ Save ]", "[ Cancel ]"].iter().enumerate() {
            viewport_text.push(line.to_string());
            for (x, ch) in line.chars().enumerate() {
                cells.push(Cell {
                    x: x as u16,
                    y: y as u16,
                    text: ch.to_string(),
                    fg: Color::unknown(),
                    bg: Color::unknown(),
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                    strike: false,
                });
            }
        }
        crate::screen::ScreenState {
            cols: 40,
            rows: 2,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells,
            viewport_text,
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    /// A snapshot declaring #save with two verbs: one conventional, one
    /// the harness has no invocation for.
    fn write_save_snapshot(path: &std::path::Path, actions: &str) {
        let body = format!(
            r##"{{"v":1,"type":"snapshot","root":{{"id":"#root","role":"screen","children":[{{"id":"#save","role":"button","label":"Save","bounds":[0,0,8,1],"actions":[{actions}]}}]}}}}"##
        );
        std::fs::write(path, format!("{body}\n")).expect("write snapshot");
    }

    #[test]
    fn declared_actions_become_affordances_in_both_shapes() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        write_save_snapshot(&path, r#""activate","export-csv""#);
        ch.poll();
        let report = ch.overlay_fused(&mut tree, &mut sem);
        assert!(
            report.matched.iter().any(|id| id == "#save"),
            "save resolved: {:?}",
            report
        );

        // Which tree node did #save land on? Native ids join by id/suffix/
        // label — a control-derived node keeps its control id, so find the
        // matched node via the native id join, not a literal id.
        fn find_native<'a>(
            n: &'a crate::semantic::node::SemanticNode,
            native_id: &str,
        ) -> Option<&'a crate::semantic::node::SemanticNode> {
            if n.identity.as_ref().and_then(|i| i.native_id.as_deref()) == Some(native_id) {
                return Some(n);
            }
            n.children.iter().find_map(|c| find_native(c, native_id))
        }
        let save = find_native(&tree.root, "#save").expect("native-joined node");

        // Tree shape: both declared verbs are affordances at native
        // confidence — including the verb with no conventional invocation
        // (honest Declared, not a guessed shape).
        let native: Vec<_> = save
            .affordances
            .iter()
            .filter(|a| a.source == "native")
            .collect();
        assert_eq!(native.len(), 2, "both declared verbs landed: {native:?}");
        assert!(native.iter().any(|a| matches!(
            a.invocation,
            crate::semantic::affordance::Invocation::Activate
        )));
        assert!(native.iter().any(|a| matches!(
            &a.invocation,
            crate::semantic::affordance::Invocation::Declared { verb } if verb == "export-csv"
        )));

        // Flat shape: the same two verbs on the screen affordance list.
        let flat: Vec<_> = sem
            .affordances
            .iter()
            .filter(|a| a.source == "native" && a.action == "Save")
            .collect();
        assert_eq!(flat.len(), 2, "flat shape carries the declared verbs");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn undeclared_activate_affordances_are_dropped_for_declared_nodes() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        // Pre-state: inference produced at least one Activate affordance on
        // this screen (the bracket buttons), so the drop below is real.
        assert!(sem.affordances.iter().any(|a| matches!(
            a.invocation,
            crate::semantic::affordance::Invocation::Activate
        )));

        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        // The app declares Save supports ONLY export — no activate.
        write_save_snapshot(&path, r#""export-csv""#);
        ch.poll();
        let _ = ch.overlay_fused(&mut tree, &mut sem);

        // Tree shape: no Activate affordance survives on the #save node —
        // the app said the verb is not supported; its word outranks
        // inference.
        fn find_native<'a>(
            n: &'a crate::semantic::node::SemanticNode,
            native_id: &str,
        ) -> Option<&'a crate::semantic::node::SemanticNode> {
            if n.identity.as_ref().and_then(|i| i.native_id.as_deref()) == Some(native_id) {
                return Some(n);
            }
            n.children.iter().find_map(|c| find_native(c, native_id))
        }
        let save = find_native(&tree.root, "#save").expect("native-joined node");
        assert!(
            !save.affordances.iter().any(|a| matches!(
                a.invocation,
                crate::semantic::affordance::Invocation::Activate
            )),
            "inferred activate must not survive an app declaration without it: {:?}",
            save.affordances
        );
        // Flat shape: Save's inferred activate affordance is gone too; only
        // the declared verb remains.
        let save_flat: Vec<_> = sem
            .affordances
            .iter()
            .filter(|a| a.action == "Save")
            .collect();
        assert!(
            !save_flat.iter().any(|a| matches!(
                a.invocation,
                crate::semantic::affordance::Invocation::Activate
            )),
            "flat: inferred activate dropped: {save_flat:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn undeclared_nodes_keep_their_inferred_affordances() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        let pre_inferred = sem.affordances.len();
        assert!(pre_inferred > 0, "inference produced affordances");

        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        write_save_snapshot(&path, r#""activate","export-csv""#);
        ch.poll();
        let _ = ch.overlay_fused(&mut tree, &mut sem);

        // Cancel was declared nothing about: its affordances survive.
        let cancel_id = sem
            .controls
            .iter()
            .find(|c| c.label == "Cancel")
            .map(|c| c.id.clone())
            .expect("cancel control exists");
        assert!(
            sem.affordances
                .iter()
                .any(|a| a.control_id.as_deref() == Some(cancel_id.as_str())),
            "undeclared controls keep their inferred affordances"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn refreshed_frame_replaces_native_affordances() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        write_save_snapshot(&path, r#""activate""#);
        ch.poll();
        let _ = ch.overlay_fused(&mut tree, &mut sem);
        // A refreshed frame declares a DIFFERENT verb set: replacement, not
        // accumulation — on both shapes. (The app appends; it does not
        // truncate the channel.)
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            f.write_all(
                r##"{"v":1,"type":"snapshot","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save","bounds":[0,0,8,1],"actions":["export-csv"]}]}}"##.as_bytes(),
            )
            .expect("append refreshed frame");
            f.write_all(b"\n").expect("newline");
        }
        ch.poll();
        let _ = ch.overlay_fused(&mut tree, &mut sem);

        fn find_native<'a>(
            n: &'a crate::semantic::node::SemanticNode,
            native_id: &str,
        ) -> Option<&'a crate::semantic::node::SemanticNode> {
            if n.identity.as_ref().and_then(|i| i.native_id.as_deref()) == Some(native_id) {
                return Some(n);
            }
            n.children.iter().find_map(|c| find_native(c, native_id))
        }
        let save = find_native(&tree.root, "#save").expect("native-joined node");
        let native: Vec<_> = save
            .affordances
            .iter()
            .filter(|a| a.source == "native")
            .collect();
        assert_eq!(native.len(), 1, "re-declaration replaces: {native:?}");
        assert!(matches!(
            &native[0].invocation,
            crate::semantic::affordance::Invocation::Declared { verb } if verb == "export-csv"
        ));
        let flat_native: Vec<_> = sem
            .affordances
            .iter()
            .filter(|a| a.source == "native" && a.action == "Save")
            .collect();
        assert_eq!(flat_native.len(), 1, "flat replaced too");
        assert!(matches!(
            &flat_native[0].invocation,
            crate::semantic::affordance::Invocation::Declared { verb } if verb == "export-csv"
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn native_only_nodes_carry_their_actions_into_the_flat_screen() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        // A control inference cannot see (no bracket text on screen).
        let ghost = r##"{"id":"#ghost","role":"button","label":"Ghost","bounds":[30,0,8,1],"actions":["summon"]}"##;
        let body = format!(
            r##"{{"v":1,"type":"snapshot","root":{{"id":"#root","role":"screen","children":[{ghost}]}}}}"##
        );
        std::fs::write(&path, format!("{body}\n")).expect("write");
        ch.poll();
        let report = ch.overlay_fused(&mut tree, &mut sem);
        assert!(report.native_only.iter().any(|id| id == "#ghost"));
        let ghost_aff: Vec<_> = sem
            .affordances
            .iter()
            .filter(|a| a.control_id.as_deref() == Some("#ghost"))
            .collect();
        assert_eq!(
            ghost_aff.len(),
            1,
            "ghost node's verb is usable, not dead data: {ghost_aff:?}"
        );
        assert!(matches!(
            &ghost_aff[0].invocation,
            crate::semantic::affordance::Invocation::Declared { verb } if verb == "summon"
        ));
        std::fs::remove_file(&path).ok();
    }

    // Finding 7: an unterminated run of garbage wider than one frame cap
    // is skipped in bounded steps (one window per poll — poll() does
    // bounded work) and counted invalid — never re-read forever, never
    // buffered whole.
    #[test]
    fn oversize_line_is_refused_and_channel_stays_live() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        // Two windows' worth of newline-less garbage, then a valid frame.
        let junk_len = MAX_FRAME_BYTES + 4096;
        let mut blob = vec![b'x'; junk_len];
        blob.push(b'\n');
        blob.extend_from_slice(
            r##"{"v":1,"type":"snapshot","root":{"id":"#after","role":"screen"}}"##.as_bytes(),
        );
        blob.push(b'\n');
        std::fs::write(&path, &blob).expect("write junk");
        ch.poll();
        assert_eq!(ch.frames_invalid, 1, "the full window was refused");
        assert_eq!(
            ch.consumed_to as usize, MAX_FRAME_BYTES,
            "advanced exactly one bounded window, not the whole line"
        );
        assert_eq!(ch.frames_accepted, 0, "no garbage parsed as a frame");
        // The next poll continues the skip and finds the valid frame
        // behind the garbage.
        ch.poll();
        assert!(ch.frames_invalid >= 2, "residual junk counted too");
        assert_eq!(ch.frames_accepted, 1, "channel recovered after the junk");
        assert_eq!(ch.latest.as_ref().expect("latest").id, "#after");
        std::fs::remove_file(&path).ok();
    }

    /// Finding 7: the oversized frame never lives in memory whole — each
    /// poll buffers at most one MAX_FRAME_BYTES window.
    #[test]
    fn oversize_read_is_bounded_by_the_frame_cap() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        let junk_len = MAX_FRAME_BYTES * 3 + 12345;
        let mut blob = vec![b'x'; junk_len];
        blob.push(b'\n');
        std::fs::write(&path, &blob).expect("write");
        ch.poll();
        assert_eq!(ch.frames_invalid, 1);
        assert_eq!(
            ch.consumed_to as usize, MAX_FRAME_BYTES,
            "one bounded window per poll — never the whole line"
        );
        // Keep polling: the skip advances window by window to the newline.
        for _ in 0..6 {
            ch.poll();
        }
        assert_eq!(ch.frames_accepted, 0);
        assert!(
            ch.frames_invalid >= 4,
            "every window counted: {}",
            ch.frames_invalid
        );
        assert_eq!(
            ch.consumed_to as usize,
            junk_len + 1,
            "the skip ended at (and consumed) the newline"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Finding 7: a non-UTF-8 line is refused as one invalid frame and the
    /// channel keeps parsing what follows (read_until semantics, not
    /// read_line error-loops).
    #[test]
    fn non_utf8_line_is_skipped_not_sticky() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        let mut blob = b"\xff\xfe garbage\n".to_vec();
        blob.extend_from_slice(
            r##"{"v":1,"type":"snapshot","root":{"id":"#ok","role":"screen"}}"##.as_bytes(),
        );
        blob.push(b'\n');
        std::fs::write(&path, &blob).expect("write");
        ch.poll();
        assert_eq!(ch.frames_invalid, 1);
        assert_eq!(ch.frames_accepted, 1);
        assert_eq!(ch.latest.as_ref().expect("latest").id, "#ok");
        std::fs::remove_file(&path).ok();
    }

    /// Finding 7: after heavy traffic the channel file compacts to the
    /// unconsumed tail — bounded disk, not O(total session traffic) — and
    /// consumption restarts from zero with nothing lost.
    #[test]
    fn channel_file_compacts_to_unconsumed_tail() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        // History: a valid frame the channel HAS consumed…
        let good = r##"{"v":1,"type":"snapshot","root":{"id":"#old","role":"screen"}}"##;
        std::fs::write(&path, format!("{good}\n")).expect("write");
        ch.poll();
        assert_eq!(ch.frames_accepted, 1);
        // …then past-threshold traffic in sub-cap complete lines (so the
        // oversize skip stays out of the way): 32 × 256 KiB of garbage.
        let junk_line = format!("y{}\n", "y".repeat(256 * 1024));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            for _ in 0..32 {
                f.write_all(junk_line.as_bytes()).expect("append junk");
            }
        }
        ch.poll();
        assert_eq!(ch.frames_invalid, 32, "every garbage line counted");
        let size_after = std::fs::metadata(&path).expect("meta").len();
        assert!(
            size_after < 1024,
            "consumed history reclaimed: file is {size_after} bytes"
        );
        assert_eq!(ch.consumed_to, 0, "offset restarted at zero");
        // …and the next write lands behind the new reader and still parses.
        let good2 = r##"{"v":1,"type":"snapshot","root":{"id":"#new","role":"screen"}}"##;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            f.write_all(good2.as_bytes()).expect("append");
            f.write_all(b"\n").expect("newline");
        }
        ch.poll();
        assert_eq!(ch.frames_accepted, 2, "post-compaction frame accepted");
        assert_eq!(ch.latest.as_ref().expect("latest").id, "#new");
        std::fs::remove_file(&path).ok();
    }

    /// Finding 7: compaction never reclaims bytes not yet consumed. The one
    /// state where bytes legitimately survive a compaction is a partial
    /// trailing line (rolled back, unconsumed) — it must be carried through
    /// the rewrite byte-for-byte and parse once the app finishes it.
    #[test]
    fn compaction_preserves_unconsumed_frames() {
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        let good = r##"{"v":1,"type":"snapshot","root":{"id":"#old","role":"screen"}}"##;
        std::fs::write(&path, format!("{good}\n")).expect("write");
        ch.poll();
        assert_eq!(ch.frames_accepted, 1);
        // Past-threshold garbage + a partial trailing frame (no newline).
        let good2 = r##"{"v":1,"type":"snapshot","root":{"id":"#pending","role":"screen"}}"##;
        let junk_line = format!("z{}\n", "z".repeat(256 * 1024));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            for _ in 0..32 {
                f.write_all(junk_line.as_bytes()).expect("append junk");
            }
            f.write_all(good2.as_bytes())
                .expect("partial frame, no newline");
        }
        ch.poll();
        assert_eq!(ch.frames_accepted, 1, "partial frame not parsed");
        assert!(
            std::fs::metadata(&path).expect("meta").len() < 1024,
            "compaction ran; only the partial tail remains"
        );
        // The app finishes the frame; it must parse from the compacted file.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            f.write_all(b"\n").expect("complete the frame");
        }
        ch.poll();
        assert_eq!(ch.frames_accepted, 2, "pending frame survived compaction");
        assert_eq!(ch.latest.as_ref().expect("latest").id, "#pending");
        std::fs::remove_file(&path).ok();
    }

    // ── Findings 16/17: honest resolution — ambiguity reported, focus exclusive ──

    /// Finding 16: a native id whose suffix matches TWO inferred nodes is
    /// ambiguous — it must NOT resolve to the first hit. The report names
    /// both competitors, and neither gains native facts. (Built directly:
    /// inference does not naturally produce colliding suffixes.)
    #[test]
    fn ambiguous_suffix_is_reported_not_first_hit() {
        use crate::semantic::node::{NodeState, SemanticNode, SemanticTree};
        let mk = |id: &str| SemanticNode {
            id: id.into(),
            parent: None,
            role: crate::semantic::node::Role::Unknown,
            bounds: crate::semantic::regions::Bounds {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            label: None,
            value: None,
            state: NodeState::default(),
            children: Vec::new(),
            affordances: Vec::new(),
            identity: None,
            confidence: crate::semantic::Confidence::inferred(0.5, &["fixture"]),
        };
        let mut tree = SemanticTree {
            root: mk("screen"),
            layers: std::collections::HashMap::new(),
        };
        tree.root.children = vec![mk("panel/a/save"), mk("panel/b/save")];

        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        let body = r##"{"v":1,"type":"snapshot","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save"}]}}"##;
        std::fs::write(&path, format!("{body}\n")).expect("write");
        ch.poll();
        let mut sem = crate::semantic::SemanticScreen {
            cols: 40,
            rows: 2,
            regions: Vec::new(),
            controls: Vec::new(),
            focus: crate::semantic::FocusInfo::default(),
            relationships: Vec::new(),
            affordances: Vec::new(),
            components: Vec::new(),
        };
        let report = ch.overlay_fused(&mut tree, &mut sem);
        // Two nodes compete for suffix `save`: the declaration is reported
        // ambiguous, not resolved to whichever came first.
        assert!(
            !report.ambiguous.is_empty(),
            "the competing match is reported: {report:?}"
        );
        let (_, competitors) = &report.ambiguous[0];
        assert_eq!(
            competitors.len(),
            2,
            "both competing ids named: {competitors:?}"
        );
        // And neither competitor gained native facts.
        fn count_native(n: &crate::semantic::node::SemanticNode, native_id: &str) -> usize {
            let own = (n.identity.as_ref().and_then(|i| i.native_id.as_deref()) == Some(native_id))
                as usize;
            own + n
                .children
                .iter()
                .map(|c| count_native(c, native_id))
                .sum::<usize>()
        }
        assert_eq!(
            count_native(&tree.root, "#save"),
            0,
            "ambiguity matched nothing"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Finding 16b: a UNIQUE suffix still resolves — the uniqueness gate
    /// must not over-refuse honest matches.
    #[test]
    fn unique_suffix_still_resolves() {
        use crate::semantic::node::{NodeState, SemanticNode, SemanticTree};
        let mk = |id: &str| SemanticNode {
            id: id.into(),
            parent: None,
            role: crate::semantic::node::Role::Unknown,
            bounds: crate::semantic::regions::Bounds {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            label: None,
            value: None,
            state: NodeState::default(),
            children: Vec::new(),
            affordances: Vec::new(),
            identity: None,
            confidence: crate::semantic::Confidence::inferred(0.5, &["fixture"]),
        };
        let mut tree = SemanticTree {
            root: mk("screen"),
            layers: std::collections::HashMap::new(),
        };
        tree.root.children = vec![mk("toolbar/save"), mk("toolbar/cancel")];

        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        let body = r##"{"v":1,"type":"snapshot","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button"}]}}"##;
        std::fs::write(&path, format!("{body}\n")).expect("write");
        ch.poll();
        let mut sem = crate::semantic::SemanticScreen {
            cols: 40,
            rows: 2,
            regions: Vec::new(),
            controls: Vec::new(),
            focus: crate::semantic::FocusInfo::default(),
            relationships: Vec::new(),
            affordances: Vec::new(),
            components: Vec::new(),
        };
        let report = ch.overlay_fused(&mut tree, &mut sem);
        assert!(
            report.matched.iter().any(|id| id == "#save"),
            "the unique suffix resolved: {report:?}"
        );
        assert!(report.ambiguous.is_empty());
        fn native_id_of(n: &crate::semantic::node::SemanticNode, want: &str) -> Option<String> {
            if n.identity.as_ref().and_then(|i| i.native_id.as_deref()) == Some(want) {
                return Some(n.id.clone());
            }
            n.children.iter().find_map(|c| native_id_of(c, want))
        }
        assert_eq!(
            native_id_of(&tree.root, "#save").as_deref(),
            Some("toolbar/save"),
            "the join landed on the ONE suffix hit"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Finding 17: focus is exclusive. When the app declares focus on ONE
    /// control, every other control's stale `focused` flag is cleared in
    /// BOTH shapes — inference's guess and an earlier frame's verdict do
    /// not survive beside the app's word.
    #[test]
    fn native_focus_clears_stale_focus_everywhere() {
        let screen = two_button_screen();
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        // Force stale focus onto Cancel (as style inference or a previous
        // frame might have left it).
        for c in sem.controls.iter_mut() {
            if c.label == "Cancel" {
                c.focused = true;
            }
        }
        // (Mark a tree node stale-focused too, so the tree-side clear is
        // proven, not just the flat side.)
        fn mark_stale_focus(n: &mut crate::semantic::node::SemanticNode, label: &str) {
            if n.label.as_deref() == Some(label) {
                n.state.focused = true;
            }
            for c in n.children.iter_mut() {
                mark_stale_focus(c, label);
            }
        }
        mark_stale_focus(&mut tree.root, "Cancel");

        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        // The app declares: SAVE is focused. Cancel's stale flag must go.
        let body = r##"{"v":1,"type":"snapshot","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save","focused":true},{"id":"#cancel","role":"button","label":"Cancel"}]}}"##;
        std::fs::write(&path, format!("{body}\n")).expect("write");
        ch.poll();
        let report = ch.overlay_fused(&mut tree, &mut sem);
        assert_eq!(report.focus_applied.as_deref(), Some("#save"));

        // Flat shape: exactly one focused control, and it is Save.
        let focused: Vec<_> = sem.controls.iter().filter(|c| c.focused).collect();
        assert_eq!(focused.len(), 1, "focus is exclusive: {focused:?}");
        assert_eq!(focused[0].label, "Save");
        assert_eq!(sem.focus.control.as_deref(), Some("Save"));

        // Tree shape: no OTHER node claims focus either — and the node
        // that does is the Save one, not the stale-marked Cancel.
        fn focused_labels(n: &crate::semantic::node::SemanticNode, out: &mut Vec<String>) {
            if n.state.focused {
                out.push(n.label.clone().unwrap_or_else(|| n.id.clone()));
            }
            for c in &n.children {
                focused_labels(c, out);
            }
        }
        let mut tree_focused = Vec::new();
        focused_labels(&tree.root, &mut tree_focused);
        assert!(
            tree_focused.len() <= 1,
            "tree focus is exclusive too: {tree_focused:?}"
        );
        assert!(
            tree_focused.first().map(String::as_str) == Some("Save"),
            "the app's verdict is the survivor: {tree_focused:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    // Fused truth (re-review Wave-4): one resolve, both shapes — the flat
    // SemanticScreen must carry the same native facts the tree carries.
    #[test]
    fn overlay_fused_writes_native_facts_into_both_shapes() {
        use crate::screen::cell::{Cell, Color, ProcessState};
        let mut cells = Vec::new();
        let mut viewport_text = Vec::new();
        for (y, line) in ["[ Save ]", "[ Cancel ]"].iter().enumerate() {
            viewport_text.push(line.to_string());
            for (x, ch) in line.chars().enumerate() {
                cells.push(Cell {
                    x: x as u16,
                    y: y as u16,
                    text: ch.to_string(),
                    fg: Color::unknown(),
                    bg: Color::unknown(),
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                    strike: false,
                });
            }
        }
        let screen = crate::screen::ScreenState {
            cols: 40,
            rows: 2,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells,
            viewport_text,
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        };
        let (mut sem, mut tree) = crate::semantic::detect_frame(&screen);
        // Whatever inference concluded (bracket-first heuristics may pick
        // Save), the native declaration below must OVERRIDE it — record the
        // pre-state only to prove the override when it differs.
        let inferred_focus = sem.focus.control.clone();

        // The app declares: cancel focused, save disabled.
        let mut ch = NativeChannel::create().expect("create");
        let path = ch.path.clone().expect("path");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                r##"{"v":1,"type":"snapshot","app":"t","framework":"test","root":{"id":"#root","role":"screen","children":[{"id":"#save","role":"button","label":"Save","enabled":false},{"id":"#cancel","role":"button","label":"Cancel","focused":true}]}}"##
            ),
        )
        .expect("write");
        ch.poll();
        assert!(ch.latest.is_some(), "snapshot accepted");

        let report = ch.overlay_fused(&mut tree, &mut sem);
        assert!(report.active(), "overlay engaged");
        // Focus: same verdict in both shapes, and in the flat focus info.
        assert_eq!(report.focus_applied.as_deref(), Some("#cancel"));
        assert!(
            report.matched.iter().any(|id| id == "#save"),
            "save matched"
        );
        // Tree side: focused node flagged.
        let cancel_node = crate::semantic::native::tests_support::find_label(&tree.root, "Cancel");
        let save_node = crate::semantic::native::tests_support::find_label(&tree.root, "Save");
        assert!(cancel_node.map(|n| n.state.focused).unwrap_or(false));
        // Save declared disabled — tree carries it with native provenance.
        assert!(
            !save_node.map(|n| n.state.enabled.value).unwrap_or(true),
            "Save declared disabled"
        );
        // Flat side: focus rewritten wholesale (label + confidence 1.0).
        assert_eq!(sem.focus.control.as_deref(), Some("Cancel"));
        assert_eq!(sem.focus.confidence, 1.0);
        assert_ne!(
            sem.focus.control, inferred_focus,
            "native declaration overrode inference"
        );
        assert!(sem.focus.evidence[0].starts_with("native-focus:"));
        // The matched control inherited native provenance.
        let save_ctrl = sem
            .controls
            .iter()
            .find(|c| c.label.contains("Save"))
            .expect("save control inferred");
        assert!(!save_ctrl.enabled, "save control disabled by native");
        assert_eq!(save_ctrl.source, "native");
        // And the tree/flat controls agree on count — same detection pass.
        assert_eq!(
            sem.controls.len(),
            crate::semantic::native::tests_support::count_controls(&tree.root),
            "flat and tree shapes come from one detection pass"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Write N synthetic event lines through the REAL poll() path and drain.
    fn ingest_events_via_poll(ch: &mut NativeChannel, path: &std::path::Path, n: usize) {
        use std::fmt::Write as _;
        let mut blob = String::new();
        for i in 0..n {
            let _ = writeln!(
                blob,
                "{}",
                serde_json::json!({
                    "v": 1,
                    "type": "event",
                    "event": "coverage",
                    "target": format!("tgt-{i}"),
                })
            );
        }
        std::fs::write(path, blob).expect("write events");
        ch.poll();
    }

    #[test]
    fn native_ring_never_stops_absorbing() {
        // The old Vec + remove(0) ring plus a positional usize cursor went
        // permanently deaf at the cap: len froze at 1024 so `total <= from`
        // held forever. With a seq-cursored VecDeque, events past the cap
        // are still delivered by events_since (re-review P0).
        let mut ch = NativeChannel::create().expect("channel");
        let path = ch.path.clone().expect("path");
        ingest_events_via_poll(&mut ch, &path, 1100);
        // Ring capped at 1024 retained events.
        assert_eq!(ch.events.len(), 1024, "ring cap enforced");
        // But all 1100 seqs were assigned, and a seq cursor of 0 sees only
        // what the ring retains (the first 76 evicted).
        assert_eq!(ch.last_native_seq(), 1100);
        assert_eq!(ch.events_since(0).len(), 1024);
        // A consumer at the pre-cap position still gets the freshest events.
        let fresh = ch.events_since(1049);
        assert_eq!(fresh.len(), 51, "events 1050..=1100 delivered after wrap");
        assert_eq!(fresh.last().expect("last").seq, 1100);
        assert_eq!(fresh.last().expect("last").target, "tgt-1099");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn native_events_since_is_exactly_once() {
        let mut ch = NativeChannel::create().expect("channel");
        let path = ch.path.clone().expect("path");
        ingest_events_via_poll(&mut ch, &path, 3);
        let first = ch.events_since(0);
        assert_eq!(first.len(), 3);
        // Absorb at the channel head, push 2 more, absorb again: EXACTLY the
        // 2 new ones — no double-count, no skip (the session's
        // absorb_native_events loop relies on this).
        let cursor = ch.last_native_seq();
        ingest_events_via_poll(&mut ch, &path, 2);
        let again = ch.events_since(cursor);
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].target, "tgt-0");
        assert_eq!(again[1].target, "tgt-1");
        // Re-reading from the same cursor yields the same batch (idempotent).
        assert_eq!(ch.events_since(cursor).len(), 2);
        std::fs::remove_file(&path).ok();
    }
}
