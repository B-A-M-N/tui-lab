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

# Check system readiness (every line is an executed probe)
hermes-tui-lab doctor

# Print version
hermes-tui-lab version

# Generate skill documentation
hermes-tui-lab skill
```

## Run lifecycle

Server startup is side-effect-free: `hermes-tui-lab mcp` opens an **ephemeral**
run — no filesystem mutation. Every ephemeral run has a real `run_id`, event
history, scenarios, checkpoints, and a state graph in memory; "ephemeral" means
"not persisted", not "feature-degraded". When a session's work becomes worth
keeping, promote the SAME run to durable storage:

```json
{ "tool": "tui_run", "arguments": { "action": "status" } }
{ "tool": "tui_run", "arguments": { "action": "persist" } }
{ "tool": "tui_run", "arguments": { "action": "close", "kill_sessions": false } }
```

- `persist` preserves run identity and everything accumulated so far (sessions,
  generations, launch specs, scenario recordings, saved scenarios, checkpoints,
  state graph, findings). The artifact root resolves from the primary session's
  `LaunchSpec.cwd` — **never** the server process's cwd — unless an explicit
  `root` is passed. With neither, it returns `invalid_request` instead of
  guessing.
- `close` flushes durable state and marks the run closed. Sessions are left
  running unless `kill_sessions: true`.
- `resume` / `list` / `compare` do not exist yet; they arrive with run
  restoration, not as stubs.

## MCP Tools

### tui_session
Manage TUI sessions: start, restart, stop, list, status.

**Actions:** `start`, `restart`, `stop`, `list`, `status`

Restart relaunches the SAME logical session: same id, next generation, reusing
the stored launch spec.

### tui_observe
Observe terminal state.

**Modes:** `summary`, `screen`, `cells`, `semantic`, `diff`, `scrollback`

`diff` compares the previous observation to the current one through the one
canonical `screen::diff`, returning the same `Transition` shape as
`tui_act` (screen + semantic diff). A session with no prior frame reports
`since: null` honestly instead of diffing a frame with itself.

### tui_act
Drive keyboard/mouse input. Returns a screen transition.

**Actions:** `key`, `keys`, `type`, `paste`, `raw`, `mouse_click`, `mouse_press`, `mouse_release`, `mouse_move`, `mouse_drag`, `mouse_scroll`, `resize`, `signal`

Each act captures the event-sequence baseline before sending, waits anchored to
that baseline, and reports `settled` honestly — plus `warnings` when the screen
did not reach stability within the settle budget. `sensitive: true` keeps the
payload out of scenario recordings.

### tui_wait
Block until a condition holds.

**Conditions:** `text`, `text_absent`, `screen_change`, `screen_stable`, `process_exit`, `title`, `bell`, `idle`

The outcome includes reason, elapsed, event sequence, and process state — not
just a boolean.

### tui_assert
Assert UI facts.

**Assertions:** `text`, `text_absent`, `position`, `focus`, `not_clipped`, `dimensions`, `exit_code`, `region`, `snapshot`, `structure`, `control_exists`, `focused_not`

Unknown assertions return `invalid_request` (caller error), not
`assertion_failed` (UI failure).

### tui_checkpoint
Save and compare named UI state checkpoints.

**Actions:** `save`, `compare`, `list`, `delete`

Checkpoints persist under the run's artifact root once the run is persistent.

### tui_scenario
Record, save, list, and export workflows as regression scenarios.

**Actions:** `record_start`, `record_stop`, `save`, `list`, `export`

`record_start` binds a recording to one session generation and returns an
opaque `recording_id` — the identity for `record_stop` (names are display
labels; two sessions can both record "login" without cross-contamination).
Subsequent `tui_act`/`tui_wait`/`tui_assert` calls resolving to that exact
session generation append steps; other sessions never do. Replay executes
steps through the same canonical executor as the MCP tools — real inputs,
real waits, real assertions.

### tui_record
Produce terminal recordings (asciinema `.cast` v3 format) from the raw PTY
byte boundary — escape sequences, timing, and intermediate frames preserved;
input bytes recorded only when requested.

### tui_explore
Seeded random exploration and candidate generation.

**Modes:** `random`, `guided_candidates`, `state_graph`

The explorer records an ordered step per action while it happens (seq, action,
before/after state identity, settle outcome, novelty) and feeds the run's state
graph from that record — the graph is what actually happened, not a post-hoc
reconstruction. Every applicable budget limit (actions, runtime, relaunches,
depth, unique states) is checked each iteration and the report names the real
`completion_reason` (`action_budget`, `time_budget`, `relaunch_budget`,
`depth_budget`, `unique_state_budget`, `clean_exit`, `failure`, `cancelled`).
Relaunches go through session restart so generation increments.

### tui_audit
Run deterministic UX audits and return evidence-backed findings.

**Profiles:** `full`, `focus`, `keyboard`, `clipping`, `resize`, `navigation`, `discoverability`

Active profiles (`keyboard`, `focus`, `resize`, `clipping`, `layout`) drive the
app through the session and observe real transitions.

### tui_coverage
Native coverage via optional tuicov executable. Reports "unavailable" honestly
when tuicov is not on PATH.

### tui_framework
Detect TUI framework and run native probes.

**Actions:** `detect`, `capabilities`

## Architecture

```
MCP → RunContext ─┬─ SessionManager → Session → TerminalBackend → PTY
                  ├─ CheckpointStore     └─ ScreenState → SemanticScreen
                  ├─ Scenario recordings (session+generation scoped)
                  ├─ StateGraph ← exploration steps (live)
                  └─ Findings ← audits
Interaction (act/wait) runs through one canonical executor (src/execution)
shared by MCP, scenario replay, audits, and exploration.
```

## Testing

The stdio E2E (`tests/mcp_stdio_e2e.rs`) spawns the real `hermes-tui-lab mcp`
binary and speaks JSON-RPC over its stdin/stdout, driving a full lifecycle
against real python3 children. It is the integration bar: a feature is not
integrated until the real MCP path can exercise it.

## Status

| Subsystem | State |
|-----------|-------|
| Terminal PTY | working |
| Screen parsing | working |
| Keyboard | working |
| Mouse | working (encoding conformance-tested) |
| Resize | working (single-record, backend-owned) |
| State waits | working (causality-anchored) |
| Semantic model | v2 (border graph) |
| MCP surface | working (12 tools, stdio E2E-proven) |
| Run lifecycle | working (ephemeral default, explicit persist/close) |
| Checkpoints | working (durable under persistent runs) |
| Scenarios | working (session+generation scoped, real replay) |
| Recording | working (asciicast v3, PTY-boundary hook) |
| Audits | working (active + static) |
| Exploration | working (seeded, live transitions, budget-authoritative) |
| Coverage | honest stub (tuicov optional executable) |
| Framework probes | working (detection) |

## License

MIT
