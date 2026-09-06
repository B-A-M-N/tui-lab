# hermes-tui-lab

Agent-native TUI instrumentation, testing, exploration, and UX evaluation harness.

## Support free inference

If you found this project useful, please consider supporting
[freeinference.org](https://freeinference.org). Open access to AI for
everyone — not just those who can pay — is an important thing, and efforts
toward it survive on community support.

**Disclaimer:** freeinference.org has not reviewed, instructed, or sponsored
this project. This is an independent effort with no affiliation, endorsement,
or involvement of any kind claimed or implied — the pointer above is a
recommendation, nothing more.

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

# Render a persisted run's recorded history (transcript, not a re-drive)
hermes-tui-lab replay <run_id> [--root <dir>] [--full]
```

## Run lifecycle

### tui_run
The run lifecycle tool — `status`, `persist`, `close`, `list`, `resume`,
`diagnose`/`repair` (per-finding diagnostic contexts), `bundle` (one
finding's context + regression diff), and `context` (the capability
registry as JSON).

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
- `list` enumerates persisted runs under a root (newest first, ledger
  transaction counts included; corrupt entries are named in place, never
  dropped).
- `resume` restores a persisted run as the server's live run — same identity,
  same artifact root. The ledger (with its declared eviction window), findings,
  checkpoints, scenarios, focus/state graphs, coverage, and launch specs come
  back; **sessions do not** — they were processes, and pretending a dead
  process came back would be the dishonesty the run model exists to prevent.
  Relaunch with `tui_session action=start` (the manifest records the specs) and
  new actions append to the same ledger.
- Closed runs stay readable without resuming: the `tui://runs/<id>` resource
  restores them read-only (`live: false`), and `hermes-tui-lab replay <run_id>`
  renders the recorded history as a transcript. Replay is a *read* of what the
  run recorded, not a re-execution — replaying inputs against live children is
  what `tui_scenario action=run` is for.

### Human control lease

`tui_session action=lease holder=<label> ttl_ms=<ms>` grants a human exclusive
control of a session (TTL-clamped 1s–1h). While the lease is live, machine
driving — `tui_act`, the driving `tui_explore`/`tui_audit` modes, scenario
replay, recording lifecycle — refuses with `control_leased` naming the holder;
observation and status stay allowed. `action=release` returns control. Acquire,
release, and gated inputs serialize through the same per-session mailbox, so
there is no check-then-race window.

### Isolation profiles

`tui_session action=start isolation=local|clean|strict` — `local` (default)
runs the child as-is; `clean` scrubs the environment; `strict` wraps the child
in `unshare --net` (no network). Every start reports `IsolationEvidence` —
what was actually applied, not what was requested.

### MCP resources (tui://)

Read-through resources over the same session/run state the tools expose:
`tui://runs/{run_id}` (live run state, or a persisted run restored read-only
with `live: false`), `tui://sessions/{session_id}/semantic`,
`tui://sessions/{session_id}/screen`, and `tui://findings`. Unknown ids are
honest `resource_not_found` errors naming what was accepted.

## MCP Tools

The tool set spans two resources surfaces (see "Run lifecycle" for `tui_run` and
"MCP resources" for the `tui://` reads; the count is registry-derived —
`tui_run action=context` reports it, and a CI test pins the registry to the
wire router. This prose is not the contract). The authoritative selector
lists live in the capability registry and the generated skill doc —
this section is prose, the registry is the contract.

### tui_session
Manage TUI sessions: start, restart, stop, list, status, plus the human
control lease.

**Actions:** `start`, `restart`, `stop`, `list`, `status`, `lease`, `release`

Restart relaunches the SAME logical session: same id, next generation, reusing
the stored launch spec. `start` accepts `isolation: local|clean|strict` (see
above) and reports the applied evidence; `lease`/`release` manage the human
control lease (see above).

### tui_observe
Observe terminal state.

**Modes:** `summary`, `screen`, `cells`, `semantic`, `tree`, `nodes`, `diff`, `changes`, `scrollback`, `search`, `command_state`, `history`

`diff` compares the previous observation to the current one through the one
canonical `screen::diff`, returning the same `Transition` shape as
`tui_act` (screen + semantic diff). A session with no prior frame reports
`since: null` honestly instead of diffing a frame with itself.

`search` scans viewport + scrollback (takes `text`/`query`); `command_state`
reports OSC 133 shell-integration state or honest `null` when the session
never emitted integration marks. When the app cooperates over
`TUI_LAB_SEMANTIC`, the app's declared tree is **fused** into semantic truth
itself — one cached detection pass builds both the flat semantic surface and
the nodes tree, then the native overlay (focus, enabled, labels, values —
`source: native`, confidence 1.0) is applied to both. `summary`, `semantic`,
`tree`, `nodes`, and the `tui://` semantic resource all report the same
verdict; `nodes` additionally names what matched and what was native-only.

`tree` returns the hierarchical terminal-state tree: regions nested per
containment, controls inside their regions with state flags, focus, and
screen-level components (tables, trees, scrollbars) attached — the
machine-readable shape to compare against an intended design. `rendered`
is an indented text view for logs.

`nodes` returns the Wave-C semantic node tree: one node type
(`{id, role, parent, children, bounds, label, value, state, affordances,
confidence}`) for everything on screen — regions, controls, table
header/rows/cells, tree items, scroll regions (with `can_scroll_up/down`,
`at_start/at_end`), text areas, selects, menus, command palettes, split
panes, toasts, OSC8 hyperlinks, help overlays, and key-hint bars. Top-level
subtrees carry layer tags (`modal` / `overlay` / `status` / `background`),
and `state.enabled` carries provenance (`source: "dim-style"` vs `"default"`
assumption) — disabled is inferred from evidence, never silently assumed.

The `semantic` payload includes the Wave-4 semantic surfaces: `affordances`
(actionable capabilities with their visible cues), `components` (detected
tables/trees/scrollbars), and `relationships` (normalized spatial relations
keyed by stable IDs with re-derivable geometry reasons).

### tui_intent
Act by semantic intent instead of by coordinates or key guesses.

**Params:** `target` (by=`id`|`text`|`role`|`focused`) + `verb`
(`activate`, `focus`, `click`, `toggle`, `select`, `open`, `type`)

The verb resolves into a focus-secured execution plan; the response names
every step and the risk class BEFORE anything is sent. `execute=true` runs
the plan — and requires the `plan_id` returned by the preview: the plan_id
binds the whole previewed plan (target, verb, risk, step shape), so a
previewed plan cannot be replayed against a different verb or control, and
a preview older than five minutes expires. Unresolved targets return
`target_error` with structured candidates, not prose. Sensitive `type`
payloads are redacted to `${NAME}` parameter references in recordings.

### tui_act
Drive keyboard/mouse input. Returns a screen transition.

**Actions:** `key`, `keys`, `type`, `paste`, `raw`, `mouse_click`, `mouse_press`, `mouse_release`, `mouse_move`, `mouse_drag`, `mouse_scroll`, `resize`, `signal`

Each act captures the event-sequence baseline before sending, waits anchored to
that baseline, and reports `settled` honestly — plus `warnings` when the screen
did not reach stability within the settle budget. `sensitive: true` keeps the
payload out of scenario recordings.

### tui_probe
Run one small experiment and get everything materially different.

One call returns: the baseline frame, the settled after-frame, the causal
events inside the probe window, the transition (with per-control render
deltas — "button/save moved x:65→71", not just `changed_cells`), and the
watched material changes. `stimulus {kind:none}` is a drift probe: watch
what changes when you change nothing. The probe's settle behavior is
declared by `completion` (stable_screen, first_change, any_change,
text_appears, text_disappears, process_exit, command_done, bell,
semantic_change, may_be_silent, no_wait) so a silent or exiting action is
never misreported.

### tui_wait
Block until a condition holds.

**Conditions:** `text`, `text_absent`, `screen_change`, `screen_stable`, `process_exit`, `title`, `bell`, `idle`, `command_done`, `command_output`

`command_done`/`command_output` anchor on OSC 133 shell-integration edges
(command_seq). A session with no integration marks fails honestly rather
than spinning.

The outcome includes reason, elapsed, event sequence, and process state — not
just a boolean.

### tui_assert
Assert UI facts.

**Assertions:** `text`, `text_absent`, `position`, `focus`, `not_clipped`, `dimensions`, `exit_code`, `region`, `snapshot`, `structure`, `control_exists`, `focused_not`, `oracle`

Unknown assertions return `invalid_request` (caller error), not
`assertion_failed` (UI failure). `oracle` evaluates the shared Wave E
expression language (the same one contracts and scenario replay use).

### tui_checkpoint
Save and compare named UI state checkpoints.

**Actions:** `save`, `compare`, `list`, `delete`

Checkpoints persist under the run's artifact root once the run is persistent.

### tui_scenario
Record, save, list, export, and replay workflows as regression scenarios.

**Actions:** `list`, `record_start`, `record_stop`, `save`, `export`, `run`

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
input bytes recorded only when requested. `format=svg|png` captures one frame
instead of a session; `format=start|stop` is the cast lifecycle. Driving the
session while recording is machine driving: blocked while a human lease is
live.

### tui_explore
Seeded random exploration, candidate generation, and screen-reading
exploration.

**Modes:** `random`, `guided_candidates`, `semantic`, `state_graph`

The explorer records an ordered step per action while it happens (seq, action,
before/after state identity, settle outcome, novelty) and feeds the run's state
graph from that record — the graph is what actually happened, not a post-hoc
reconstruction. Every applicable budget limit (actions, runtime, relaunches,
depth, unique states) is checked each iteration and the report names the real
`completion_reason` (`action_budget`, `time_budget`, `relaunch_budget`,
`depth_budget`, `unique_state_budget`, `clean_exit`, `failure`, `cancelled`).
Relaunches go through session restart so generation increments.

`guided_candidates` is evidential: every suggested action cites where its
novelty claim comes from — the state graph (`"tab" has never been executed
from this state`), the design contract (`declared but never exercised`), an
on-screen affordance, or interaction coverage (`control has never held
focus`). `max_risk` (`safe` < `mutating` < `destructive` <
`external_side_effect`) filters candidates above the allowance entirely — a
`safe` explorer is never offered "activate Delete Database". `semantic` mode
loops: read the screen, execute the top evidential candidate through the
canonical executor, record the transition, repeat — deterministic for a given
app state, unlike the seeded random fuzz.

### tui_audit
Run deterministic UX audits and return evidence-backed findings.

**Profiles:** `full`, `keyboard`, `focus`, `resize`, `layout`, `clipping`, `discoverability`, `navigation`, `contract`, `color`, `performance`, `mouse`, `states`, `errors`

Active profiles (keyboard, focus, resize/layout, clipping, navigation, mouse,
color, performance, states, errors, and `full` as the composite) drive the app
through the session and observe real transitions — which makes them machine
driving: blocked while a human lease is live (observational readers — color,
rendering, terminal_modes, input_protocol, shell_cli, lifecycle,
query_response — stay allowed during a lease). Static profiles
(`discoverability`) read the current screen only. The safe-only default
withholds invasive profiles (an ORCH-GATED finding names how to allow them)
until `allow_mutation=true`; `restart_between_mutations=true` additionally
restart-replays between mutating drivers (requires `allow_mutation=true`;
restarting the app does not undo external side effects). `lifecycle_exit`
consumes the target and needs `allow_process_restart=true`. Label passes
with `label=` and diff them with `compare_to=`.
`navigation` is the traversal-proof audit: it drives Tab forward and Shift+Tab
back, records every transition into the ID-keyed focus graph, and reports the
proven Tab order, the wrap-around cycle, and — edge for edge — whether
Shift+Tab truly reverses Tab (missing inverses are named by control ID, not
shrugged at).

### tui_explain
Explain one audit finding: each evidence ref traced to its source, plus the
terminal capabilities (via the live profile) the finding is conditional on.
Also available read-only as the `tui://findings/{finding_id}` resource.

### tui_workflow
The construction workflow per finding — one object per answer, no autonomy.

**Actions:** `inspect`, `verify`, `diagnose`

`inspect` assembles the whole chain in one object: finding → component
identity → source loci (provenance-tiered) → framework context (rooted at
the run's recorded launch cwd, with `root_provenance` naming where the root
came from) → contract expectation → minimal reproduction → targeted
validation. `verify` runs that verification plan live — replay the recorded
reproduction, re-run the finding's cheapest audit surface — and reports
whether the finding still reproduces (lease-gated when the surface drives).
`diagnose` lists every finding's chain. Nothing here edits code or prescribes
fixes; everything cites the run's own evidence.

### tui_coverage
Coverage with two providers, merged honestly. Native coverage: a cooperative
app sends `{"type":"event","event":"coverage","target":"…"}` frames over the
`TUI_LAB_SEMANTIC` side channel; the run ledger aggregates them per session
and `action=ledger` returns the entries (hits, sessions, first/last seen) —
so an agent can check whether an interaction exercised new application code.
Optional tuicov executable: `delta`/`uncovered`/`start`/`stop` invoke it for
normalized JSON and fail with `unsupported` when it is not on PATH — never a
fabricated percentage.

### tui_framework
Detect TUI framework, run native probes, and hand out adapter snippets.

**Actions:** `detect`, `capabilities`, `adapter_snippet`

`adapter_snippet` returns a ready-to-paste NativeSemanticProtocol declarer
for the detected framework (Ratatui, Textual, or the dependency-free Python
reference in `fixtures/nsproto.py`): the app declares its widget tree over
the side channel and the harness's inference yields to it.

### tui_contract
Design contracts: describe what the TUI is *supposed* to be in YAML, and check
the running app against that description.

**Actions:** `load`, `validate`, `status`, `compare`

A contract declares viewports the layout must survive, components that must be
present, interactions (a key sequence plus the oracle expressions that must
hold after it), layout constraints, and standalone oracles:

```yaml
schema: { name: my-tui, version: "1" }
escape_closes_modal: true
reverse_tab_required: true
volatile_patterns: ['\bCPU \d+%']
components:
  - { name: main-table, role: table, required: true }
interactions:
  - name: open-confirm
    keys: ["n"]
    expect: ['modal_open()', 'text_present("Confirm")']
oracles:
  - { id: host-field, expr: 'visible("Host")' }
```

`status` runs the full conformance check and returns PASS / WARN / FAIL per
check group (document, component, interaction, layout, behavior) — driving the
app where the claim requires it: interactions send their keys, the Escape and
reverse-Tab properties are proven by observation, never assumed. `compare`
diffs the current conformance against a stored baseline and names every
regression (Pass→Fail) and fix (Fail→Pass); regressions become findings.
Loading a contract also installs its `volatile_patterns` into the session's
normalization policy, so declared-volatile tokens stop fragmenting structure
hashes and the state graph.

The oracle language (`focused("#field/host")`, `modal_open()`,
`escape_closes_modal()`, `reverse_tab_is_inverse()`, `no_clipping()`,
`text_present("...")`, …) is shared by contracts, `tui_assert
assertion=oracle`, and scenario replay — one assertion language, three
consumers. `tui_audit profile=contract` runs the loaded contract's checks and
folds failures into the findings ledger; a loaded contract also feeds
`tui_explore`: declared-but-never-exercised keys join the candidate queue
citing the contract as evidence.

## Architecture

```
MCP → RunContext ─┬─ SessionPool → per-session actor thread → Session → TerminalBackend → PTY
                  ├─ CheckpointStore     └─ ScreenState → SemanticScreen
                  ├─ Scenario recordings (session+generation scoped)
                  ├─ StateGraph ← exploration steps (live)
                  └─ Findings ← audits
Interaction (act/wait) runs through one canonical executor (src/execution)
shared by MCP, scenario replay, audits, and exploration. Each session is a
thread-backed actor with a bounded mailbox: handlers ship a closure to the
session's own thread, so a slow session blocks only its own callers and the
run-context lock is never held across an actor call. The human control lease
is checked inside the actor job, so lease acquire/release and gated input
serialize through the same mailbox.
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
| Keyboard | working (legacy + xterm modified-navigation `CSI 1;<m>` + kitty keyboard protocol: app-pushed `CSI > flags u` promotes the capability and unlocks CSI-u encoding incl. SUPER and F13–F20; SUPER rejected without it) |
| Mouse | working (SGR/X10/UTF-8 byte-conformance-tested: press/release/move/drag/wheel) |
| Resize | working (single-record, backend-owned) |
| State waits | working (causality-anchored; + `command_done`/`command_output` on OSC 133 shell-integration edges) |
| Scrollback | working (captured by paging the parser buffer; searchable across viewport+history; capability promoted only after rows are real) |
| Terminal queries | working (responder: DA1/DA2/DA3, DSR 5n/6n, DECRQM, kitty `?u`, OSC 10/11 color reports — answered with actually-true state) |
| Line CLI backend | working (second engine behind the same trait for non-screen CLIs; pipes not PTY; line history with honest capabilities; select via `backend: "cli"`) |
| Semantic model | v3 (border graph + SemanticNode tree, modal layering, widget families, provenance-tracked enabled, OSC8 hyperlinks) |
| Native semantic protocol | working (TUI_LAB_SEMANTIC NDJSON side-channel: app-declared trees overlay inference with source=native/confidence=1.0; adapters + `tui_framework action=adapter_snippet` for Ratatui/Textual/Python) |
| Screen capture | working (SVG with style runs + PNG via dependency-free encoder; `tui_record format=svg\|png`) |
| MCP surface | working (registry-derived tool count + tui:// resources, stdio E2E-proven; tool/resource docs generated from the capability registry, count pinned to the router by test) |
| Human control lease | working (TTL grant, exclusive; every driving path refuses with control_leased, observation stays allowed) |
| Isolation profiles | working (local/clean/strict with IsolationEvidence; strict wraps the child in unshare --net) |
| Run lifecycle | working (ephemeral default, persist keeps identity, close, list, resume restores ledger/findings/checkpoints/scenarios/graphs — not sessions; tui://runs reads closed runs read-only) |
| Checkpoints | working (durable under persistent runs) |
| Scenarios | working (session+generation scoped, real replay) |
| Recording | working (asciicast v3, PTY-boundary hook) |
| Audits | working (active + static + contract conformance) |
| Contracts | working (YAML/JSON load, oracle language, conformance PASS/FAIL/WARN, compare) |
| Exploration | working (seeded, live transitions, budget-authoritative, contract-fed) |
| Coverage | working (native NSP coverage events → run ledger with per-session correlation; optional tuicov executable invoked on demand, absent = honest Unsupported) |
| Framework probes | working (detection + adapter snippets) |
| Run replay CLI | working (`hermes-tui-lab replay <run_id>` renders the persisted ledger/findings/graphs — a read of what was recorded, not a re-drive) |

## License

MIT
