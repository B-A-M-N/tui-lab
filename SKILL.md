# tui-lab skill

MCP server for agent-native TUI instrumentation, testing, exploration, and UX evaluation.

## Tools

- `tui_session` — Manage TUI sessions: start, restart, stop, list, status, plus the human control lease (lease/release).
  - action: start, restart, stop, list, status, lease, release, attach
- `tui_observe` — Observe terminal state: summary, screen text, cells, semantic surfaces, node tree, diffs, scrollback, search, shell-command state, protocol trace + mode timeline (portable-pty/line engines), pipe stdout/stderr streams, and inspect — the one-call construction view (frame + semantic identity, per-control stable ids/bounds/state/affordances/source loci, native overlay health, loaded-contract violations; target= narrows to one control).
  - mode: summary, screen, cells, semantic, tree, nodes, diff, changes, scrollback, search, command_state, history, protocol, streams, terminal_modes, inspect
- `tui_act` — Drive input through the canonical executor: key, keys, type, paste, raw, mouse_click/press/release/move/drag/scroll, resize, signal (tagged union schema). Optional `completion` declares how "done" means (stable_screen/first_change/any_change/text_appears/text_disappears/process_exit/command_done/bell/semantic_change/may_be_silent/no_wait) so a silent/exit action is never misreported as settled=false.
- `tui_intent` — Act by semantic intent: resolve a target (by=id|text|role|focused) + verb (activate/focus/click/toggle/select/open/type) into a focus-secured execution plan; the response names every step and the risk class before anything is sent. execute=true runs the plan; unresolved targets return target_error with structured candidates (details), not prose.
- `tui_wait` — Block until a condition holds; conditions anchor on causality (action baselines) or shell-integration command edges.
  - condition: text, text_absent, screen_change, screen_stable, process_exit, title, bell, idle, command_done, command_output, event
- `tui_probe` — Run one small experiment and get EVERYTHING materially different: baseline vs settled after-frame, causal events inside the probe window, transition (with finding-40 control_deltas — per-control WHAT changed, e.g. 'button/save moved x:65→71', not just changed_cells), watched material changes. stimulus {kind:none} = drift probe.
  - completion: stable, first_change, any_change, text_appears, text_disappears, process_exit, semantic_change, may_be_silent
- `tui_assert` — Assert UI facts (text, text_absent, position, focus, focused_not, not_clipped, dimensions, exit_code, region, snapshot, structure, control_exists); unknown assertions are invalid_request (caller error), never assertion_failed (UI failure). `oracle` evaluates the shared Wave E language.
  - assertion: text, text_absent, position, focus, not_clipped, dimensions, exit_code, region, snapshot, structure, control_exists, focused_not, oracle
- `tui_checkpoint` — Save and compare named UI state checkpoints (durable under persistent runs).
  - action: save, compare, list, delete
- `tui_scenario` — Record, save, list, export, and replay interaction scenarios (session+generation scoped); regression_asset synthesizes review-gated regression assets (scenario/assertion/contract_rule/viewport_case) from a finding's own evidence (finding 39) — generated/inferred, never auto-run.
  - action: list, record_start, record_stop, save, export, run, regression_asset
- `tui_record` — Capture terminal output: asciicast .cast lifecycle (start/stop) plus one-shot SVG/PNG screen captures.
  - format: start, stop, cast, svg, png
- `tui_explore` — Seeded random exploration, evidential candidate generation, screen-reading semantic exploration, and the state graph (modes: random, guided_candidates, semantic, state_graph). Driving: blocked while a human lease is live. Replay of discovered flows is tui_scenario's job.
  - mode: random, guided_candidates, semantic, state_graph
- `tui_audit` — Deterministic UX audits returning evidence-backed findings; `full` is the composite of every non-process-consuming family. label=/compare_to= diff findings across runs. Safe-only default: invasive profiles are withheld (ORCH-GATED) until allow_mutation=true; restart_between_mutations=true restart-replays between mutating drivers (with allow_mutation=true; not an external-side-effect boundary). Driving profiles are blocked while a human lease is live; observational readers stay allowed. lifecycle_exit consumes the target and needs allow_process_restart=true.
  - profile: full, keyboard, focus, resize, layout, clipping, discoverability, navigation, contract, color, performance, mouse, states, errors, unicode, controls, terminal_modes, rendering, input_protocol, shell_cli, lifecycle, lifecycle_exit, query_response
- `tui_coverage` — Coverage: native NSP coverage-event ledger plus the optional tuicov executable (honest Unsupported when absent).
  - action: detect, summary, collect, delta, ledger, snapshot
- `tui_framework` — Framework detection, capability probes, and NativeSemanticProtocol adapter snippets (Ratatui/Textual/Python).
  - action: detect, capabilities, adapter_snippet
- `tui_run` — Run lifecycle: status, persist (ephemeral→durable, same identity), close, list persisted runs, resume one as the live run, diagnose (or its alias repair): diagnostic evidence contexts per finding — provenance-tiered source loci, verification plan (targeted checks, replay only with a reproduction), observation-shaped next steps — never edit prescriptions, bundle for ONE finding (context + before/after regression diff), and context (this registry as JSON).
  - action: status, persist, close, context, list, resume, diagnose, new, bundle
- `tui_contract` — Design contracts: load, validate, conformance status, baseline compare (regressions become findings), and scaffold — generate a starter contract from the LIVE observed frame (scaffold_mode=current) or from a bounded SAFE multi-state pass — initial screen, Tab focus walk, Escape, viewport probes (scaffold_mode=explore; lease-gated; every state cited in the scaffold.inferred extension; edit from observation toward intent).
  - action: load, validate, status, compare, scaffold, baseline
- `tui_explain` — Explain an audit finding: trace each evidence ref to its source and flag terminal capabilities (via the live profile) the finding is conditional on.
- `tui_workflow` — Construction workflow per finding (one object, no autonomy): inspect assembles finding → component identity → source loci → framework context → contract expectation → minimal reproduction → targeted validation; verify runs that verification plan live (replay + re-checks; lease-gated) and reports whether the finding still reproduces; diagnose lists every finding's chain.
  - action: construct, inspect, verify, diagnose
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
resource or `tui-lab replay <run_id>` — a transcript render, not a
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
- `tui://runs/{run_id}/timeline` — First-class causal timeline over the retained transaction window: dispatch provenance, generation, event anchors, before/after frame references, settlement, and render citations joined per transaction.
- `tui://runs/{run_id}/timeline/{seq}` — One joined causal timeline entry by transaction seq: the primary debugging artifact for a single interaction.
- `tui://runs/{run_id}/scenarios` — Saved scenarios in a run (review P1 evidence-addressability): ids, names, step counts, and the per-scenario URI. Live runs read memory+disk; persisted runs are restored read-only.
- `tui://runs/{run_id}/scenarios/{scenario_id}` — One scenario by id (or unambiguous name) — the full recorded step list, addressable as evidence.
- `tui://runs/{run_id}/transactions` — The declared-replay transaction ledger (bounded retained window + lifetime count). Citable as the run's interaction history.
- `tui://runs/{run_id}/transactions/{seq}` — One transaction by ledger seq: action, settle verdict, before/after structure, changed cells, render evidence.
- `tui://runs/{run_id}/frames` — Committed frame records in the hot ring (audit P0-11): every `frame:N` cited by timeline entries is a registered, resolvable resource. Evicted ids resolve through frames.jsonl on persistent runs.
- `tui://runs/{run_id}/frames/{frame_id}` — One frame record by citable id (hot ring first, then frames.jsonl): frame_id, session/generation provenance, screen/output seqs, structure/visual/semantic identity, commit time.
- `tui://sessions/{session_id}/semantic` — Live semantic screen: regions, controls, focus, affordances, components.
- `tui://sessions/{session_id}/screen` — Live screen text + geometry.
- `tui://sessions/{session_id}/terminal-profile` — Evidence-backed terminal capability report (review §12): reads the live backend capabilities without forcing a screen settle — observationally pure, unlike the screen-backed views.
- `tui://findings` — Findings accumulated this run (audits, contracts, exploration).
- `tui://findings/{finding_id}` — One finding by instance id, rendered as its explanation (review P1 evidence-addressability): evidence refs traced to sources — the tui_explain shape, as a read-only resource.
