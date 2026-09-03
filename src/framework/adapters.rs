//! Framework adapter snippets (Wave F items 58–63).
//!
//! The NativeSemanticProtocol is framework-agnostic; these snippets show
//! the per-framework wiring. They ship as *text* (returned by
//! `tui_framework action=adapter_snippet`) because the target application
//! must adopt them into its own source — the harness cannot (and must not)
//! inject code into the child. Snippets are documentation-grade: complete
//! enough to paste, small enough to review.

/// The ratatui (Rust) snippet: a `native_semantic_snapshot` helper to call
/// after every draw.
pub const RATATUI: &str = r##"// NativeSemanticProtocol adapter for Ratatui (Wave F items 58-63).
// Write your REAL widget tree to the TUI_LAB_SEMANTIC side channel after
// each draw; the tui-lab harness merges it over inference (source: native,
// confidence 1.0).Apps that never do this still work — this is additive.
//
// Cargo.toml: serde = { version = "1", features = ["derive"] }, serde_json = "1"

use serde::Serialize;

#[derive(Serialize)]
struct NspNode<'a> {
    id: &'a str,
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bounds: Option<[u16; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focusable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Vec<NspNode<'a>>,
}

#[derive(Serialize)]
struct NspFrame<'a> {
    v: u32,
    #[serde(rename = "type")]
    frame_type: &'a str,
    app: &'a str,
    framework: &'a str,
    root: NspNode<'a>,
}

/// Call after `terminal.draw(...)` with your real widget tree.
pub fn native_semantic_snapshot(path: &std::path::Path, root: NspNode) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        use std::io::Write;
        let frame = NspFrame {
            v: 1,
            frame_type: "snapshot",
            app: "my-tui",
            framework: "ratatui",
            root,
        };
        if let Ok(mut line) = serde_json::to_string(&frame) {
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
    }
}

// Where `path` comes from: std::env::var("TUI_LAB_SEMANTIC"). If unset,
// skip entirely — the harness is not attached.
//
// Example (a two-button footer):
//   let path = match std::env::var("TUI_LAB_SEMANTIC") { Ok(p) => p.into(), Err(_) => return };
//   native_semantic_snapshot(&path, NspNode {
//       id: "#root", role: "screen", label: None, bounds: Some([0, 0, area.width, area.height]),
//       actions: None, focusable: None, focused: None, enabled: None,
//       children: vec![
//           NspNode { id: "#save", role: "button", label: Some("Save"),
//               bounds: Some([1, h-2, 6, 1]), actions: Some(vec!["activate"]),
//               focusable: Some(true), focused: Some(focus == 0), enabled: Some(true),
//               children: vec![] },
//           /* ... */
//       ],
//   });
"##;

/// The Textual (Python) snippet: a mixin that declares the tree Textual
/// already maintains internally.
pub const TEXTUAL: &str = r##"# NativeSemanticProtocol adapter for Textual (Wave F items 58-63).
# Textual KNOWS its widget tree; this mixin declares it to the tui-lab
# harness after every render. Drop-in: class MyApp(NSPMixin, App): ...
# Without TUI_LAB_SEMANTIC set, everything is a no-op.

import json, os

ROLE_MAP = {
    "Button": "button", "Input": "field", "CheckBox": "checkbox",
    "RadioSet": "radio", "Static": "label", "DataTable": "table",
    "Tree": "tree", "ListView": "list", "TextArea": "text_area",
    "Select": "select", "Tabs": "tab", "TabbedContent": "panel",
}


class NSPMixin:
    """Declare the real Textual widget tree over the semantic side channel."""

    def _nsp_path(self):
        return os.environ.get("TUI_LAB_SEMANTIC")

    def _nsp_node(self, widget):
        from textual.widget import Widget
        role = ROLE_MAP.get(type(widget).__name__, "widget")
        node = {
            "id": f"#{widget.id}" if widget.id else f"@{type(widget).__name__}/{id(widget):x}",
            "role": role,
        }
        label = getattr(widget, "label", None) or getattr(widget, "value", None)
        if isinstance(label, str):
            node["label"] = label
        try:
            r = widget.region
            node["bounds"] = [r.x, r.y, r.width, r.height]
        except Exception:
            pass
        node["focusable"] = widget.can_focus
        node["focused"] = widget.has_focus
        node["enabled"] = not widget.has_class("-disabled")
        children = [self._nsp_node(c) for c in widget.children]
        if children:
            node["children"] = children
        return node

    def _nsp_declare(self):
        path = self._nsp_path()
        if not path:
            return
        try:
            frame = {
                "v": 1,
                "type": "snapshot",
                "app": getattr(self, "TITLE", "textual-app"),
                "framework": "textual",
                "root": self._nsp_node(self.screen),
            }
            with open(path, "a", encoding="utf-8") as f:
                f.write(json.dumps(frame, separators=(",", ":")) + "\n")
        except OSError:
            pass  # a broken side channel must never break the app

    # Textual render hook: called after each frame is composed.
    def on_mount(self):
        self.set_interval(0.2, self._nsp_declare)
        # Preserve any existing on_mount in other bases (Textual supports
        # multiple handlers via message bubbling; direct subclasses that
        # define their own on_mount should call super().on_mount()).
"##;

/// The reference Python snippet (for raw-ANSI / curses apps): a self-contained,
/// paste-ready version of the app-side client. The full test implementation
/// lives in `fixtures/nsproto.py`; this kept copy is what
/// `tui_framework action=adapter_snippet source=python` returns, so
/// `adapter_snippet` always yields real source the user can adopt — never a
/// comment pointer to a fixture the harness may or may not have.
pub const PYTHON_REFERENCE: &str = r##"#!/usr/bin/env python3
# NativeSemanticProtocol reference adapter (raw-ANSI / curses / any Python TUI).
# Call snapshot() after every render with your REAL node tree; tui-lab merges
# it over inference. If TUI_LAB_SEMANTIC is unset, everything is a no-op, so
# this is safe to leave in an app that also runs standalone.

import json
import os

ENV_VAR = "TUI_LAB_SEMANTIC"


def node(id, role, label=None, bounds=None, actions=None,
         focusable=None, focused=None, enabled=None, children=None):
    """Build one native node dict."""
    out = {"id": id, "role": role}
    for key, val in (("label", label), ("bounds", bounds), ("actions", actions),
                     ("focusable", focusable), ("focused", focused),
                     ("enabled", enabled), ("children", children)):
        if val is not None:
            out[key] = val
    return out


class Client:
    """Append-only writer for the TUI_LAB_SEMANTIC channel."""

    def __init__(self, app, framework=None):
        self.path = os.environ.get(ENV_VAR)
        self.enabled = self.path is not None
        self.app = app
        self.framework = framework

    def _write(self, frame):
        if not self.enabled:
            return
        frame = {"v": 1, **frame}
        try:
            with open(self.path, "a", encoding="utf-8") as f:
                f.write(json.dumps(frame, separators=(",", ":")) + "\n")
        except OSError:
            pass  # a broken side channel must never break the app

    def snapshot(self, root):
        self._write({"type": "snapshot", "app": self.app,
                     **({"framework": self.framework} if self.framework else {}),
                     "root": root})

    def event(self, event, target):
        self._write({"type": "event", "event": event, "target": target})


# Example: declare a two-button footer after your draw.
# ns = Client("my-tui", framework="curses")
# ns.snapshot(node("#root", "screen", children=[
#     node("#save", "button", label="Save", focusable=True,
#          focused=focus == 0, actions=["activate"]),
#     node("#cancel", "button", label="Cancel", focusable=True,
#          focused=focus == 1, actions=["activate"]),
# ]))
"##;

/// Return the adapter snippet for a framework slug.
pub fn snippet_for(framework: &str) -> Option<&'static str> {
    match framework {
        "ratatui" => Some(RATATUI),
        "textual" => Some(TEXTUAL),
        "python" | "raw" | "curses" | "reference" => Some(PYTHON_REFERENCE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_exist_for_advertised_frameworks() {
        assert!(snippet_for("ratatui").is_some());
        assert!(snippet_for("textual").is_some());
        assert!(snippet_for("python").is_some());
        assert!(snippet_for("cobol").is_none());
    }

    #[test]
    fn snippets_carry_the_protocol_contract() {
        for fw in ["ratatui", "textual"] {
            let s = snippet_for(fw).expect("snippet");
            assert!(s.contains("TUI_LAB_SEMANTIC"), "{fw}: names the env var");
            assert!(
                s.contains("\"snapshot\"") || s.contains("'snapshot'"),
                "{fw}: snapshot frame"
            );
        }
    }

    /// Review P1 item 15 — adapter snippets must EMIT source, not point at
    /// it: every advertised framework returns paste-ready code (real function
    /// bodies and frame names), never a comment-only stub or a pointer to a
    /// fixture the user may not have. A regression to "look at fixtures/x.py"
    /// must fail here.
    #[test]
    fn every_advertised_snippet_emits_real_source() {
        for fw in ["ratatui", "textual", "python", "raw", "curses", "reference"] {
            let s = snippet_for(fw).unwrap_or_else(|| panic!("{fw}: snippet available"));
            let body = s.trim();
            assert!(!body.is_empty(), "{fw}: not blank");
            // Source-bearing: a real body defines a callable or a frame
            // (`def`/`fn`/`class`/struct) AND names the protocol frame. A
            // comment-only pointer would satisfy neither.
            let defines_callable = body.contains("fn ")
                || body.contains("def ")
                || body.contains("class ")
                || body.contains("struct ");
            assert!(defines_callable, "{fw}: snippet must define real source");
            // Every family must produce the snapshot write, not just an env var.
            let emits_snapshot =
                body.contains("snapshot") && (body.contains("type") && body.contains("snapshot"));
            assert!(emits_snapshot, "{fw}: must emit a snapshot frame");
        }
    }
}
