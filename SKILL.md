# hermes-tui-lab skill

MCP server for agent-native TUI instrumentation, testing, exploration, and UX evaluation.

## Tools

- `tui_session` — Manage TUI sessions: start, restart, stop, list, status, plus the human control lease (lease/release).
  - action: start, restart, stop, list, status, lease, release
- `tui_observe` — Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state, protocol trace + mode timeline (portable-pty/line engines), pipe stdout/stderr streams.
  - mode: summary, screen, cells, semantic, tree, nodes, diff, changes, scrollback, search, command_state, history, protocol, streams, terminal_modes
- `tui_act` — Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema). Optional `completion` declares how "done" means (stable_screen/first_change/any_change/text_appears/text_disappears/process_exit/command_done/bell/semantic_change/may_be_silent/no_wait) so a silent/exit action is never misreported as settled=false.
- `tui_wait` — Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.
  - condition: text, text_absent, screen_change, screen_stable, process_exit, title, bell, idle, command_done, command_output
- `tui_probe` — Run one small experiment and get EVERYTHING materially different: baseline vs settled after-frame, causal events inside the probe window, transition, watched anomalies. stimulus {kind:none} = drift probe.
  - completion: stable, first_change, any_change, text_appears, text_disappears, process_exit, semantic_change, may_be_silent
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
- `tui_run` — Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, repair packets for every finding, and context (this registry as JSON).
  - action: status, persist, close, context, list, resume, repair
- `tui_contract` — Design contracts: load, validate, conformance status, and baseline compare (regressions become findings).
  - action: load, validate, status, compare
- `tui_explain` — Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities (via the live profile) the finding is conditional on.


## Core principle: OBSERVE BEFORE ACTING, DIFF AFTER

- Prefer `tui_observe` modes `summary` and `semantic` over `screen`. Never ask
  for a full cell dump unless you specifically need it.
- `tui_act` returns a **transition diff by default** (changed cells, focus
  change, new/removed text). Read that, not a fresh full screen.
- Never use fixed `sleep`. Use `tui_wait` conditions. The engine synchronizes
  on terminal events, not wall-clock time.

## The run model

Every session belongs to a **run**. Runs are ephemeral by default (nothing is
written to disk); `tui_run action=persist` makes the run durable under
`.tui-lab/runs/<run_id>` while keeping its identity, `action=close` ends it,
`action=list` enumerates persisted runs, and `action=resume` restores a
persisted run as the live run (the ledger, findings, checkpoints, scenarios,
graphs, and coverage come back; sessions do not — relaunch them with
`tui_session action=start`; the launch specs are recorded in the run
manifest). Read closed runs without resuming them via the `tui://runs/<id>`
resource or `hermes-tui-lab replay <run_id>` — a transcript render, not a
re-drive.

## Human control lease

`tui_session action=lease holder=<label> ttl_ms=<ms>` grants a human exclusive
control of a session. While the lease is live, machine driving (`tui_act`,
driving `tui_explore`/`tui_audit` modes, `tui_scenario` run, `tui_record`
lifecycle) refuses with `control_leased`; observation and status stay allowed.
`tui_session action=release` gives control back. Lease acquire/release and
gated inputs serialize through the same per-session mailbox, so there is no
check-then-race window.

## Isolation profiles

`tui_session action=start isolation=local|clean|strict` — `local` (default)
runs the child as-is; `clean` scrubs the environment; `strict` wraps the child
in `unshare --net` (no network). Each start reports `IsolationEvidence` (what
was actually applied), not a claim.

## NativeSemanticProtocol (TUI_LAB_SEMANTIC)

Apps that know their own UI can declare it: the harness injects a per-session
channel path via the `TUI_LAB_SEMANTIC` env var; the app writes NDJSON
snapshots (`{"v":1,"type":"snapshot",...}`) and events. Declared focus,
enabled, labels, and values override inference (source=native, confidence 1.0).
Malformed frames are counted, never fatal; the harness never writes into the
channel. Get the adapter snippet with `tui_framework action=adapter_snippet`.

## Key facts

- Errors are categorized (`invalid_request` for caller mistakes, honest
  `unsupported`/`control_leased` for refusals); unknown selector values always
  name the accepted list.
- Findings carry `evidence` (scenario, action index, before/after focus,
  reproduction id). Act on evidence, not prose.
- `tui_run action=context` returns this capability registry as JSON.

## Resources (tui://)

- `tui://runs/{run_id}` — Run status + manifest. Live runs read live state; persisted runs are restored read-only from disk (live=false).
- `tui://sessions/{session_id}/semantic` — Live semantic screen: regions, controls, focus, affordances, components.
- `tui://sessions/{session_id}/screen` — Live screen text + geometry.
- `tui://findings` — Findings accumulated this run (audits, contracts, exploration).

For detailed tool schemas, generate with `hermes-tui-lab skill`.

