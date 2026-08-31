# hermes-tui-lab skill

MCP server for agent-native TUI instrumentation, testing, exploration, and UX evaluation.

## Tools

- `tui_session` — start/stop TUI sessions
- `tui_observe` — capture screen state
- `tui_act` — drive keyboard/mouse input
- `tui_wait` — synchronize on terminal events
- `tui_assert` — verify UI properties
- `tui_checkpoint` — save/restore screen snapshots
- `tui_scenario` — record/replay interaction sequences
- `tui_record` — capture terminal sessions as asciinema
- `tui_explore` — seeded random exploration, evidential candidate generation, screen-reading semantic exploration
- `tui_audit` — run UX audits (keyboard/focus/clipping/resize drive the live app; navigation proves Tab order + Shift+Tab reversal via the ID-keyed focus graph)
- `tui_coverage` — collect coverage data
- `tui_framework` — native framework inspection

For detailed tool schemas, generate with `hermes-tui-lab skill`.
