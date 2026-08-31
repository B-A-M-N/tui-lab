#!/usr/bin/env python3
"""NativeSemanticProtocol reference adapter (Wave F items 58-63).

A cooperative TUI writes its real semantic tree to the file named by the
TUI_LAB_SEMANTIC env var (injected by the harness). This module is the
dependency-free reference implementation for Python TUIs:

    import nsproto
    ns = nsproto.Client("my-tui", framework="textual")
    ns.snapshot(nsproto.node("#root", "screen", children=[
        nsproto.node("#save", "button", label="Save", focusable=True,
                     focused=focus_idx == 0, actions=["activate"]),
    ]))
    ns.event("focus", "#save")

Semantics:
- `snapshot` replaces the app's whole declared tree (call after every render).
- `event` records a transient fact (focus/activate) between snapshots.
- Writing is append-only NDJSON; the harness tails the file, so this is
  safe from inside any render loop.
- If TUI_LAB_SEMANTIC is unset, everything is a no-op — the app runs
  unmodified outside the harness.
"""

import json
import os

ENV_VAR = "TUI_LAB_SEMANTIC"
VERSION = 1


def node(id, role, label=None, value=None, bounds=None, actions=None,
         focusable=None, focused=None, enabled=None, children=None):
    """Build one native node dict."""
    return {
        "id": id,
        "role": role,
        **({"label": label} if label is not None else {}),
        **({"value": value} if value is not None else {}),
        **({"bounds": list(bounds)} if bounds is not None else {}),
        **({"actions": list(actions)} if actions is not None else {}),
        **({"focusable": focusable} if focusable is not None else {}),
        **({"focused": focused} if focused is not None else {}),
        **({"enabled": enabled} if enabled is not None else {}),
        **({"children": list(children)} if children else {}),
    }


class Client:
    """Append-only writer for the TUI_LAB_SEMANTIC channel."""

    def __init__(self, app, framework=None, path=None):
        self.path = path or os.environ.get(ENV_VAR)
        self.app = app
        self.framework = framework
        self.enabled = self.path is not None

    def _write(self, frame):
        if not self.enabled:
            return
        frame = {"v": VERSION, **frame}
        try:
            with open(self.path, "a", encoding="utf-8") as f:
                f.write(json.dumps(frame, separators=(",", ":")) + "\n")
        except OSError:
            # A broken side channel must never break the app.
            pass

    def snapshot(self, root):
        """Declare the app's current full semantic tree."""
        self._write({
            "type": "snapshot",
            "app": self.app,
            **({"framework": self.framework} if self.framework else {}),
            "root": root,
        })

    def event(self, event, target):
        """Record a transient fact (focus, activate, ...)."""
        self._write({"type": "event", "event": event, "target": target})
