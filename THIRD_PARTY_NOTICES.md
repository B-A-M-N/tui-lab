# Third-Party Notices

`tui-lab` does not vendor any third-party source code. The following projects
were used as *conceptual* and *behavioral* references only, and their ideas were
reimplemented behind `tui-lab`'s own interfaces (the `TerminalBackend` trait,
the `ScreenState`/`SemanticScreen` models, and the MCP tool surface). Their
licenses are recorded here per the project's source/license position.

| Project            | Role in tui-lab                                   | License            |
|--------------------|---------------------------------------------------|--------------------|
| Microsoft `tui-test` | Runtime substrate concept (PTY/emulator/waits/records); pinned behind `TerminalBackend` | MIT |
| `rmcp` (rust-sdk)  | MCP server framework (stdio transport, tool macros) | MIT / Apache-2.0 |
| `tui_mcp`          | Interaction/API ergonomics (named keys, waits, text-first observation) | MIT / Apache-2.0 |
| `mcp-tui-test`     | Testing vocabulary (assert_contains, get_region, stream-vs-screen) | MIT |
| `tuibot`           | Exploration/replay concepts (seeded random, run bundles, minimization) | MIT |
| `tuicov`           | Optional coverage adapter (executable probe only) | MIT |
| GitHub `TUIkit`    | Design-contract concepts (component/behavior specs) | MIT |
| `portable-pty`     | Fallback PTY engine (used as the default substrate) | Apache-2.0 / MIT |
| `vt100`            | Fallback terminal emulator (used as the default substrate) | MIT |

## Dependency licenses (Cargo)

- `rmcp`, `schemars`, `serde`, `serde_json`, `tokio`, `anyhow`, `thiserror`,
  `tracing`, `uuid`, `regex`, `rand` — MIT / Apache-2.0 as per their crates.
- `portable-pty` — Apache-2.0 OR MIT.
- `vt100` — MIT.

No source code from the above was copied. Where behavior was modeled (e.g.
`tui_mcp`'s wait conditions, `tuibot`'s seeded exploration, `tui-test`'s stable
error categories), it was reimplemented to fit `tui-lab`'s architecture.
