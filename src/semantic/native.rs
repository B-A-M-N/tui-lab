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
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
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
                apply_native_facts(n, node);
                report.matched.push(node.id.clone());
            } else {
                report.native_only.push(node.id.clone());
            }
        }
        report
    }

    /// Fused merge (re-review Wave-4): resolve each native node ONCE and
    /// write it into both shapes — the [`SemanticTree`] (nodes mode) and the
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
        for (_path, node) in &flat {
            report.native_ids.push(node.id.clone());
            let mut resolved = resolve_node(&mut tree.root, node);
            // Capture the join facts before the mutable move below — the
            // tree node's id is the key back into `sem.controls`.
            let resolved_id: Option<String> = resolved.as_ref().map(|n| n.id.clone());
            let resolved_label: Option<String> =
                resolved.as_ref().and_then(|n| n.label.clone());
            if node.focused == Some(true) {
                if let Some(n) = resolved.as_deref_mut() {
                    n.state.focused = true;
                    report.focus_applied = Some(node.id.clone());
                }
                // Fused: focus is mode-independent. Resolve the label from
                // the tree node when the native node carries none (apps
                // often declare ids only).
                sem.focus = crate::semantic::FocusInfo {
                    control: node.label.clone().or(resolved_label),
                    control_id: Some(resolved_id.clone().unwrap_or_else(|| node.id.clone())),
                    confidence: 1.0,
                    evidence: vec![format!("native-focus:{}", node.id)],
                };
            }
            if let Some(n) = resolved {
                apply_native_facts(n, node);
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
                    report.native_only.push(node.id.clone());
                } else {
                    // No bounds: a pointer without geometry cannot be placed;
                    // it stays report-only, honestly.
                    report.native_only.push(node.id.clone());
                }
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

/// Apply one native node's declared facts onto its resolved tree node.
/// Shared by [`NativeChannel::overlay`] and
/// [`NativeChannel::overlay_fused`] — the two entry points must never drift
/// apart in what they write.
fn apply_native_facts(n: &mut crate::semantic::node::SemanticNode, node: &NativeNode) {
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

/// The flat-shape counterpart of [`role_from_slug`].
fn kind_from_slug(slug: &str) -> Option<crate::semantic::controls::ControlKind> {
    use crate::semantic::controls::ControlKind;
    Some(match slug.to_lowercase().as_str() {
        "button" => ControlKind::Button,
        "field" | "textbox" | "input" => ControlKind::Field,
        "checkbox" => ControlKind::Checkbox,
        "radio" => ControlKind::Radio,
        "tab" => ControlKind::Tab,
        "list" => ControlKind::List,
        "menuitem" | "menu_item" => ControlKind::MenuItem,
        _ => return None,
    })
}

/// Re-review P0 (native-only insertion): place an unmatched native node into
/// the inferred tree. Attach under the smallest containing node (root when
/// nothing contains it); returns `false` when the node has no usable bounds.
fn insert_native_only(
    root: &mut crate::semantic::node::SemanticNode,
    native: &NativeNode,
) -> bool {
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
        role: role_from_slug(&native.role)
            .unwrap_or(crate::semantic::node::Role::Unknown),
        parent: None,
        bounds: crate::semantic::regions::Bounds { x, y, width: w, height: h },
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
        confidence: crate::semantic::Confidence::native(),
    }
}

/// The flat-shape counterpart: a native-only node also becomes a Control so
/// flat-mode consumers (summary/semantic views) see the same truth.
fn native_node_to_control(native: &NativeNode) -> Option<crate::semantic::controls::Control> {
    let [x, y, w, h] = native.bounds?;
    Some(crate::semantic::controls::Control {
        id: native.id.clone(),
        kind: kind_from_slug(&native.role)
            .unwrap_or(crate::semantic::controls::ControlKind::Button),
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
    node.children
        .iter()
        .find_map(|c| find_by_suffix_key(c, want))
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
        node.children
            .iter()
            .find_map(|c| find_label(c, label))
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
        assert!(report.matched.iter().any(|id| id == "#save"), "save matched");
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
}
