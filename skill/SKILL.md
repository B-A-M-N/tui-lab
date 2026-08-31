---
name: tui-lab
description: Use when auditing, driving, exploring, or testing a terminal UI (TUI) through the hermes-tui-lab MCP server. Covers launching a TUI, semantic observation, automated interaction, UX audits, coverage-guided exploration, and regression scenarios — all WITHOUT a vision model.
---

# tui-lab — Agent-Native TUI Instrumentation & UX Harness

`tui-lab` (binary `hermes-tui-lab`) is a local MCP server that gives Hermes a
Playwright-for-terminals: a stable session model over a real PTY, semantic
inference from the cell grid, state-aware waits, assertions, UX audits,
seeded exploration, and optional coverage via `tuicov`. It does NOT require a
vision model — the canonical observation source is terminal state (cells,
attributes, cursor, title), not screenshots.

## When to use

- Auditing a TUI for UX problems (focus traps, clipping, broken Tab order,
  unreachable controls, resize regressions, missing visible actions).
- Driving a TUI to reproduce or verify a bug.
- Exploring a TUI workflow to discover what it can do.
- Generating regression scenarios from discovered interactions.
- Collecting native coverage (when `tuicov` is installed) to know whether an
  interaction exercised new application code.

## Core principle: OBSERVE BEFORE ACTING, DIFF AFTER

- Prefer `tui_observe` modes `summary` and `semantic` over `screen`. Never ask
  for a full 160×50 cell dump unless you specifically need it.
- `tui_act` returns a **transition diff by default** (changed cells, focus
  change, new/removed text). Read that, not a fresh full screen.
- Never use fixed `sleep`. Use `tui_wait` conditions (`text`, `screen_stable`,
  `screen_change`, `process_exit`, `title`, `bell`). The engine synchronizes
  on terminal events, not wall-clock time.

## Hermes workflow (the agent development loop)

1. `tui_framework action=detect` — identify the framework (ratatui/bubbletea/
   textual/ink/...) and whether a native adapter is available.
2. `tui_session action=start` — launch the TUI. Returns capability detection
   (mouse, kitty keyboard, colors, scrollback, title, framework, coverage).
3. `tui_observe mode=summary` then `mode=semantic` — establish a baseline.
   The semantic model reports regions (dialog/panel/...), controls
   (button/field/checkbox/...), and the focused control, each with a
   `confidence` and `source: "inferred"`. Treat inference as a hypothesis, not
   ground truth.
   For structure-aware work prefer `mode=nodes`: the Wave-C semantic node
   tree expresses everything as one tree (screen → dialog → table → rows →
   cells), tags modal vs overlay vs status layers, reports scroll edges
   ("can_scroll_down: true" instead of blind PageDown), OSC8 hyperlinks,
   and disabled/read-only state with provenance (`source: "dim-style"` vs
   `"default"` assumption).
4. Explore: `tui_act` + `tui_wait` to walk a workflow. After each action read
   the transition. Use `tui_explore mode=guided_candidates` to get ranked
   next actions (Hermes reasons; the tool does NOT embed another LLM).
5. `tui_audit profile=full` (or a specific profile: keyboard/focus/layout/
   resize/navigation/contract/discoverability/color/density) — returns
   evidence-backed findings with IDs, severities, and reproduction scenarios.
6. Edit the TUI source to fix findings.
7. Rebuild, then `tui_scenario action=run` the exact failing scenario(s) to
   confirm the fix. `tui_audit` again and compare: FIXED / NEW / UNCHANGED.
8. `tui_audit profile=resize` runs the default viewport matrix (60×20, 80×24,
   100×30, 120×40, 160×50) and flags clipping / unreachable controls.
9. Contract-driven development: author a YAML contract describing what the
   TUI is SUPPOSED to be (`tui_contract action=load path=app.contract.yaml`),
   then `tui_contract action=status` for PASS/FAIL/WARN per check —
   components present, interactions producing their declared oracles, layout
   surviving declared viewports, Escape-closes-modal and Shift+Tab-reverses-
   Tab proven by driving the app. `tui_contract action=compare` after a fix
   names every regression and every fix. Oracle expressions
   (`modal_open()`, `focused("#button/save")`, `no_clipping()`,
   `escape_closes_modal()`, …) are shared by contracts, `tui_assert
   assertion=oracle`, and scenario replay steps. Loading a contract also
   installs its `volatile_patterns` into the structure-hash normalization
   policy and feeds declared-but-unexercised keys into `tui_explore`.
10. Run broader `tui_explore mode=random seed=...` (deterministic) and
   `tui_coverage` if available. Stop when acceptance gates pass.

## Tools

- `tui_session` — Manage TUI sessions: start, restart, stop, list, status, plus the human control lease (lease/release).
  - action: start, restart, stop, list, status, lease, release
- `tui_observe` — Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state.
  - mode: summary, screen, cells, semantic, tree, nodes, diff, changes, scrollback, search, command_state, history
- `tui_act` — Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema).
- `tui_wait` — Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.
  - condition: text, text_absent, screen_change, screen_stable, process_exit, title, bell, idle, command_done, command_output
- `tui_assert` — Assert UI facts; unknown assertions are invalid_request (caller error), never assertion_failed (UI failure). `oracle` evaluates the shared Wave E language.
  - assertion: text, text_absent, position, focus, not_clipped, dimensions, exit_code, region, snapshot, structure, control_exists, focused_not, oracle
- `tui_checkpoint` — Save and compare named UI state checkpoints (durable under persistent runs).
  - action: save, compare, list, delete
- `tui_scenario` — Record, save, list, export, and replay interaction scenarios (session+generation scoped).
  - action: list, record_start, record_stop, save, export, run
- `tui_record` — Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures.
  - format: start, stop, cast, svg, png
- `tui_explore` — Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, and the state graph. Driving: blocked while a human lease is live.
  - mode: random, guided_candidates, semantic, state_graph
- `tui_audit` — Deterministic UX audits returning evidence-backed findings; `full` is the composite. label=/compare_to= diff findings across runs. Active profiles drive the app: blocked while a human lease is live.
  - profile: full, keyboard, focus, resize, layout, clipping, discoverability, navigation, contract, color, performance, mouse, states, errors
- `tui_coverage` — Coverage: native NSP coverage-event ledger plus the optional tuicov executable (honest Unsupported when absent).
  - action: detect, summary, collect, delta, uncovered, ledger, start, stop
- `tui_framework` — Framework detection, capability probes, and NativeSemanticProtocol adapter snippets (Ratatui/Textual/Python).
  - action: detect, capabilities, adapter_snippet
- `tui_run` — Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, and context (this registry as JSON).
  - action: status, persist, close, context, list, resume
- `tui_contract` — Design contracts: load, validate, conformance status, and baseline compare (regressions become findings).
  - action: load, validate, status, compare

## Resources (tui://)

- `tui://runs/{run_id}` — Run status + manifest. Live runs read live state; persisted runs are restored read-only from disk (live=false).
- `tui://sessions/{session_id}/semantic` — Live semantic screen: regions, controls, focus, affordances, components.
- `tui://sessions/{session_id}/screen` — Live screen text + geometry.
- `tui://findings` — Findings accumulated this run (audits, contracts, exploration).

## Key facts

- `supports_parallel_tool_calls: false` is REQUIRED — operations share one
  mutable terminal session and must stay serialized.
- MCP sampling must stay disabled; Hermes controls the reasoning loop.
- Findings carry `evidence` (scenario, action index, before/after focus,
  reproduction id). Act on evidence, not prose.
- Do NOT claim visual correctness from terminal attributes alone. Use framework
  native probes when available; request a screenshot only when cell/attr data
  is insufficient.
- Convert discovered failures into `tui_scenario` saves so fixes are verifiable.
- Coverage joins an `action → screen transition → new coverage?` loop; a
  `tuicov` executable is the only coverage source and is optional.

## Configuration (Hermes)

```yaml
mcp_servers:
  tui_lab:
    command: "/home/USER/.local/bin/hermes-tui-lab"
    args: ["mcp"]
    supports_parallel_tool_calls: false
    tools:
      include:
        - tui_session
        - tui_observe
        - tui_act
        - tui_wait
        - tui_assert
        - tui_checkpoint
        - tui_scenario
        - tui_record
        - tui_explore
        - tui_audit
        - tui_coverage
        - tui_framework
        - tui_run
        - tui_contract
    sampling:
      enabled: false
```

Install: build `cargo build --release` and copy `target/release/hermes-tui-lab`
to `~/.local/bin/`.
