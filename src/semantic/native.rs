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
//!   never writes into the channel.
//! - Malformed frames are skipped and counted (`frames_invalid`) — a buggy
//!   adapter degrades to inference, never breaks observation.
//! - Native data is merged over inference with `source: "native"` and
//!   `confidence: 1.0`; matching is by role/label/bounds containment. A
//!   native node that matches nothing is still reported (in a separate
//!   `native_only` list) — never silently dropped.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// Protocol version understood by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// The env var injected into session children.
pub const ENV_VAR: &str = "TUI_LAB_SEMANTIC";

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
    let f: NativeFrame = serde_json::from_str(line)
        .map_err(|e| format!("invalid frame: {e}"))?;
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
    /// Focus/activate events as (at_ms, event, target).
    pub events: Vec<(u64, String, String)>,
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
            use std::io::Seek;
            let mut f = f;
            if f.seek(std::io::SeekFrom::Start(self.consumed_to)).is_err() {
                return;
            }
            self.reader = Some(BufReader::new(f));
        }
        let Some(reader) = self.reader.as_mut() else { return };
        loop {
            let mut line = String::new();
            let before_read = self.consumed_to;
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF; keep reader for next poll
                Ok(_) => {
                    // A frame is only complete when newline-terminated; a
                    // partial trailing line stays unread until the app
                    // finishes writing it. Roll the offset back to the
                    // start of that line and drop the reader — its buffer
                    // may hold bytes the app rewrites, so the next poll
                    // must reopen at the rolled-back offset.
                    if !line.ends_with('\n') {
                        self.consumed_to = before_read;
                        self.reader = None;
                        break;
                    }
                    self.consumed_to += line.len() as u64;
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
                                    self.events.push((
                                        now,
                                        frame.event.unwrap_or_default(),
                                        frame.target.unwrap_or_default(),
                                    ));
                                    // Bounded event log.
                                    if self.events.len() > 1024 {
                                        self.events.remove(0);
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
            let mut resolved = resolve_node(&mut tree.root, node);
            // Focus: the app's word outranks inference.
            if node.focused == Some(true) {
                if let Some(n) = resolved.as_deref_mut() {
                    n.state.focused = true;
                    report.focus_applied = Some(node.id.clone());
                }
            }
            if let Some(n) = resolved {
                // Enable provenance: app-declared enabled is native evidence.
                if let Some(enabled) = node.enabled {
                    n.state.enabled =
                        crate::semantic::node::EnabledState {
                            value: enabled,
                            source: "native".to_string(),
                            confidence: 1.0,
                        };
                }
                if let Some(label) = &node.label {
                    if n.label.is_none() {
                        n.label = Some(label.clone());
                    }
                }
                if let Some(value) = &node.value {
                    n.value = Some(value.clone());
                }
                report.matched.push(node.id.clone());
            } else {
                report.native_only.push(node.id.clone());
            }
        }
        report
    }

    /// Reset (session restart): keep the path, drop the state.
    pub fn reset(&mut self) {
        self.latest = None;
        self.events.clear();
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
    // mutable borrow across the later lookups.
    enum Key {
        Id(String),
        Suffix(String),
    }
    let key = if find_by_key(root, &native.id, 0).is_some() {
        Key::Id(native.id.clone())
    } else {
        let want = native.id.trim_start_matches(['#', '@']).to_lowercase();
        if !want.is_empty() && find_by_suffix_key(root, &want).is_some() {
            Key::Suffix(want)
        } else if let Some(label) = &native.label {
            let mut hits: Vec<&crate::semantic::node::SemanticNode> = Vec::new();
            collect_by_label(root, label, &mut hits);
            if hits.len() == 1 {
                Key::Id(hits[0].id.clone())
            } else {
                return None;
            }
        } else {
            return None;
        }
    };
    match key {
        Key::Id(id) => find_by_id(root, &id),
        Key::Suffix(want) => find_by_suffix(root, &want),
    }
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

fn find_by_suffix_key<'a>(
    node: &'a crate::semantic::node::SemanticNode,
    want: &str,
) -> Option<&'a crate::semantic::node::SemanticNode> {
    let last = node.id.rsplit('/').next().unwrap_or("").to_lowercase();
    if last == want {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_by_suffix_key(c, want))
}

fn find_by_suffix<'a>(
    node: &'a mut crate::semantic::node::SemanticNode,
    want: &str,
) -> Option<&'a mut crate::semantic::node::SemanticNode> {
    let last = node.id.rsplit('/').next().unwrap_or("").to_lowercase();
    if last == want {
        return Some(node);
    }
    for c in node.children.iter_mut() {
        if let Some(found) = find_by_suffix(c, want) {
            return Some(found);
        }
    }
    None
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
        std::fs::write(&path, format!("{}\n", r##"{"v":1,"type":"snapshot","root":{"id":"#r","role":"screen"}}"##)).expect("write full");
        ch.poll();
        assert_eq!(ch.frames_accepted, 1);
        std::fs::remove_file(&path).ok();
    }
}
