# hermes-tui-lab

Agent-native TUI instrumentation, testing, exploration, and UX evaluation harness.

## Installation

```bash
cargo install --path .
```

## Usage

```bash
# Start MCP server over stdio
hermes-tui-lab mcp

# Check system readiness
hermes-tui-lab doctor

# Print version
hermes-tui-lab version

# Generate skill documentation
hermes-tui-lab skill
```

## MCP Tools

### tui_session
Manage TUI sessions: start, restart, stop, list, status.

**Actions:** `start`, `restart`, `stop`, `list`, `status`

### tui_observe
Observe terminal state.

**Modes:** `summary`, `screen`, `cells`, `semantic`, `diff`, `scrollback`

### tui_act
Drive keyboard/mouse input. Returns a screen transition.

**Actions:** `key`, `keys`, `type`, `paste`, `raw`, `mouse_click`, `mouse_press`, `mouse_release`, `mouse_move`, `mouse_drag`, `mouse_scroll`, `resize`, `signal`

### tui_wait
Block until a condition holds.

**Conditions:** `text`, `text_absent`, `screen_change`, `screen_stable`, `process_exit`

### tui_assert
Assert UI facts.

**Assertions:** `text`, `text_absent`, `position`, `focus`, `not_clipped`, `dimensions`, `exit_code`, `region`, `snapshot`, `structure`

### tui_checkpoint
Save and compare named UI state checkpoints.

**Actions:** `save`, `compare`, `list`, `delete`

### tui_scenario
Record, save, list, and export workflows as regression scenarios.

**Actions:** `save`, `list`, `export`

### tui_record
Produce terminal recordings (asciinema `.cast` v3 format).

### tui_explore
Seeded random exploration and candidate generation.

**Modes:** `random`, `guided_candidates`

### tui_audit
Run deterministic UX audits and return evidence-backed findings.

**Profiles:** `full`, `focus`, `keyboard`, `clipping`, `resize`, `navigation`, `discoverability`

### tui_coverage
Native coverage via optional tuicov executable.

### tui_framework
Detect TUI framework and run native probes.

**Actions:** `detect`, `capabilities`

## Architecture

```
MCP → Session → TerminalBackend → PTY → ScreenState → SemanticScreen → Audit/Exploration
```

## Status

| Subsystem | State |
|-----------|-------|
| Terminal PTY | working |
| Screen parsing | working |
| Keyboard | working |
| Mouse | experimental |
| Resize | working |
| State waits | working |
| Semantic model | v2 (border graph) |
| MCP surface | working |
| Checkpoints | working |
| Scenarios | working |
| Recording | working (asciicast v3) |
| Audits | working (active + static) |
| Exploration | working (seeded + state graph) |
| Coverage | stub |
| Framework probes | working (detection) |

## License

MIT
