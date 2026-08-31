# hermes-tui-lab skill

MCP server for agent-native TUI instrumentation, testing, exploration, and UX evaluation.

## Tools

- `tui_session` — start/stop TUI sessions (`backend: "auto"|"portable_vt100"|"cli"` — the CLI engine runs non-screen programs like git/npm on pipes with line history)
- `tui_observe` — capture screen state (`scrollback`, `search`, `command_state` modes; `nodes` overlays app-declared trees from the TUI_LAB_SEMANTIC side channel)
- `tui_act` — drive keyboard/mouse input (SUPER/F13+ encode as kitty CSI-u when the app pushed the protocol; rejected otherwise — honestly)
- `tui_wait` — synchronize on terminal events (incl. `command_done`/`command_output` on OSC 133 shell-integration edges)
- `tui_assert` — verify UI properties
- `tui_checkpoint` — save/restore screen snapshots
- `tui_scenario` — record/replay interaction sequences
- `tui_record` — capture terminal sessions as asciinema, or snapshot a frame as SVG/PNG (`format: "svg"|"png"`) for human debugging
- `tui_explore` — seeded random exploration, evidential candidate generation, screen-reading semantic exploration
- `tui_audit` — run UX audits (keyboard/focus/clipping/resize drive the live app; navigation proves Tab order + Shift+Tab reversal via the ID-keyed focus graph)
- `tui_coverage` — coverage: native NSP coverage-event ledger (+`action: "ledger"`), optional tuicov executable (honest Unsupported when absent)
- `tui_framework` — framework detection, capability probes, and `adapter_snippet` (Ratatui/Textual/Python) for the NativeSemanticProtocol side channel

## NativeSemanticProtocol (TUI_LAB_SEMANTIC)

Apps that know their own UI can declare it: the harness injects a per-session
channel path via the `TUI_LAB_SEMANTIC` env var; the app writes NDJSON
snapshots (`{"v":1,"type":"snapshot",...}`) and events. Declared focus,
enabled, labels, and values override inference (source=native, confidence 1.0).
Malformed frames are counted, never fatal; the harness never writes into the
channel. Get the adapter snippet with `tui_framework action=adapter_snippet`.

For detailed tool schemas, generate with `hermes-tui-lab skill`.
