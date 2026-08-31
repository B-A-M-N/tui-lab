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

/// The reference Python snippet (for raw-ANSI / curses apps): points at the
/// shipped `nsproto.py` module.
pub const PYTHON_REFERENCE: &str = r##"#!/usr/bin/env python3
# NativeSemanticProtocol reference adapter (see fixtures/nsproto.py).
# import nsproto
# ns = nsproto.Client("my-tui", framework="curses")
# ns.snapshot(nsproto.node("#root", "screen", children=[ ... ]))
# ns.event("focus", "#save")
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
}
