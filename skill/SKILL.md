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
4. Explore: `tui_act` + `tui_wait` to walk a workflow. After each action read
   the transition. Use `tui_explore mode=guided_candidates` to get ranked
   next actions (Hermes reasons; the tool does NOT embed another LLM).
5. `tui_audit profile=full` (or a specific profile: keyboard/focus/layout/
   resize/navigation/discoverability/color/density) — returns evidence-backed
   findings with IDs, severities, and reproduction scenarios.
6. Edit the TUI source to fix findings.
7. Rebuild, then `tui_scenario action=run` the exact failing scenario(s) to
   confirm the fix. `tui_audit` again and compare: FIXED / NEW / UNCHANGED.
8. `tui_audit profile=resize` runs the default viewport matrix (60×20, 80×24,
   100×30, 120×40, 160×50) and flags clipping / unreachable controls.
9. Run broader `tui_explore mode=random seed=...` (deterministic) and
   `tui_coverage` if available. Stop when acceptance gates pass.

## Tools (12)

- `tui_session` — start / restart / stop / list / status.
- `tui_observe` — summary | screen | cells | region | semantic | diff |
  scrollback | history.
- `tui_act` — key / keys / type / paste / mouse_click / mouse_move / mouse_drag
  / mouse_scroll / resize / signal / raw. Auto-waits + captures transition.
- `tui_wait` — text / text_absent / screen_change / screen_stable /
  process_exit / title / bell / command_complete.
- `tui_assert` — text / text_absent / position / region / foreground /
  background / style / cursor / focus / dimensions / snapshot / structure /
  exit_code / title / not_clipped / inside / above / below / left_of /
  right_of / aligned.
- `tui_checkpoint` — save / compare / list / delete (screen + semantic + focus
  + process + coverage state).
- `tui_scenario` — start_recording / stop_recording / save / load / run /
  compare / export (turns workflows into deterministic regression tests).
- `tui_record` — cast / svg_sequence / apng / gif / mp4.
- `tui_explore` — random / guided_candidates / coverage_guided / replay.
- `tui_audit` — keyboard | focus | layout | resize | navigation |
  discoverability | states | errors | mouse | color | performance | full.
- `tui_coverage` — detect / start / collect / summary / delta / uncovered /
  stop (backed by optional `tuicov` executable; reports "unavailable" if absent).
- `tui_framework` — detect / capabilities / inspect_native / generate_tests /
  run_native_tests.

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
    sampling:
      enabled: false
```

Install: build `cargo build --release` and copy `target/release/hermes-tui-lab`
to `~/.local/bin/`.
