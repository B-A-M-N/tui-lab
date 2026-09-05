# God-Object Refactoring Tracker

## Scope

Refactor the five god objects identified in the public-beta review:

1. **TuiLabServer** (`src/mcp/tools.rs`) — 4,165 lines → handler families
2. **RunContext** (`src/run/mod.rs`) — 3,455 lines → composed stores
3. **Session** (`src/session/state.rs`) — 1,472 lines → composed internal state
4. **AuditProfile** (`src/audit/orchestrator.rs`) — 1,358 lines → single descriptor
5. **Params** (`src/mcp/params.rs`) — 1,908 lines → canonical + generated

## Status (all five phases complete; suite green at every gate)

| Phase | Target | Result |
| --- | --- | --- |
| 1 | RunContext impl-family decomposition | done — `src/run/*_impl.rs` (9 family modules, 69 methods); mod.rs 3,344 → 1,684 |
| 2 | Session impl-family decomposition | done — `src/session/state/*.rs` (5 family modules, 77 methods, `#[path]` decls); state.rs 1,472 → 538 |
| 3 | Data-driven AuditProfileDescriptor table | done — `PROFILES` in orchestrator.rs is the single source of truth (parse/name/risk/lease/full/transaction/order); `tests/audit_profile_parity.rs` pins engine ↔ MCP |
| 4 | params.rs split by handler family | done — `src/mcp/params/` (12 modules + mod.rs re-exports; external paths unchanged) |
| 5 | MCP tools.rs handler families | done — 16 thin delegates + `tools/handlers/` (11 family modules); tools.rs 4,187 → ~840 |

Also landed alongside (review findings fixed on the new structure):

- §5 lease gate on `requires_exclusive_control()` (not overloaded `is_active()`)
- §6 `lifecycle_exit` MCP-selectable behind explicit `allow_process_restart=true`; never in `full`
- §8 `plan_intent` multi-step focus-secured activation (EnsureFocus → AssertFocus → Act)
- §13 caller-owned coverage delta cursor (`since_seq`)
- §12 `terminal-profile` in resource discovery/registry/SKILL
- §9 probe `anomalies` → `material_changes`
- §16 doc/description drift sweep (16-tool count, tui_session/tui_run/tui_coverage wording, SKILL.md regenerated sections)
- §4 source-attribution provenance tiers (`attested`/`correlated`/`inferred`/`unknown`; `is_actionable()` requires attested — correlated 0.7 loci no longer clear the fence)
- §2/§3 repair→diagnosis recontracting (`DiagnosticContext`, `VerificationPlan` decoupled from reproduction; `tui_run action=diagnose` with `repair` as alias; observation-shaped next steps)

## Design Decisions

- Keep `RunContext` as the transaction/composition boundary — don't replace with independent mutexes
- Keep `Session` as the per-session concurrency authority — no independent locking
- Don't expose `lifecycle_exit` through MCP without explicit safety gate
- `full` = all standard non-terminal-consuming audit families (fix wording)
- Provenance gates actionability; confidence only refines (review §4)
- `repair` stays an accepted alias for `diagnose`; the response's `contract: "diagnostic"` field names the recontracted meaning (review §2)

## Execution Strategy

Phase 1: RunContext decomposition (foundational — everything else depends on it)
Phase 2: Session decomposition (parallelizable after phase 1 interfaces)  
Phase 3: MCP handler families (depends on phase 1 + 2 interfaces)
Phase 4: Params + AuditProfile descriptors (parallel to phases 1-3)
Phase 5: Integration + tests

## Guard Rails

- Every phase must compile (`cargo check`) before proceeding
- E2E tests must pass after full refactoring
- No behavioral changes in phases 1–5 (mechanical only); review fixes are separately gated by tests
- Preserve all public APIs that external code depends on
- Comments/doc strings preserved or migrated

## Known follow-ups (not blockers)

- SKILL.md regeneration is a manual splice: `cargo run -- skill` only PRINTS the
  doc (it is compiled in via `include_str!`, so the generator cannot rewrite its
  own source). The parity test catches drift; a real generator command would
  close the loop.
- `plan_intent` is implemented and tested engine-side but not yet exposed
  through MCP (a `tui_act` semantic branch or `tui_interact` tool would carry
  it); the review's precondition — multi-step focus-secured plans — is met.

## Post-review P1 round (2026-09)

All nine P1 items from the re-review are implemented:

1. **§14 structured error payloads** — `Envelope.details`
   (`Option<serde_json::Value>`) populated at the lease gate
   (holder/retry_after_ms), stale-state guards (expected/actual), target
   resolution (candidates/matches), and invalid selectors (field/got/
   alternatives).
2. **`tui_intent` MCP tool** — plan/execute surface over `plan_intent`:
   plan-only returns steps + risk before anything is sent; `execute=true`
   runs the focus-secured sequence (EnsureFocus → observe → AssertFocus →
   Act). `tests/intent_mcp_test.rs` covers the wire contract.
3. **Evidence-addressable resources** — `tui://findings/{id}`,
   `tui://runs/{id}/scenarios[/{sid}]`,
   `tui://runs/{id}/transactions[/{seq}]`; live and persisted runs both
   resolve (persisted read-only).
4. **God-object residue splits** — `src/audit/driver.rs` (3,669 lines) →
   `driver/` family files + `shared.rs` helpers; `src/execution/mod.rs`
   (1,945) → `record`/`transaction`/`executor`/`wait`; `src/intent.rs`
   (1,026) → `vocab`/`plan`/`keys`; `handlers/observe.rs` (702) →
   `observe.rs` dispatcher + `observe_modes.rs`. All `crate::…` paths
   preserved via re-exports; the semantic authority gate's file list now
   names the split driver files.
5. **`cargo run -- skill --write`** — splices the generated Tools/Resources
   sections into SKILL.md on disk (idempotent; drift loop closed).
6. (dup of 2)
7. **Coverage honesty in `tui_run action=context`** — the registry context
   names the ledger/NSP path alongside the optional tuicov executable.
8. **`is_active()` → `needs_live_session()`** — §5 vocabulary finished.
9. **wave3c flake** — `wave3c_profiles_read_real_traffic` settles with an
   observe cycle after pool.start, same treatment as the earlier
   query_response fix.

## Beta-stable audit map (2026-09-04)

Status of the 44-finding beta-stable release audit, verified against code
at `84ef926`. The earlier architecture-audit commits (`9a9c484`..`b1559d5`)
covered much of the same ground under a different numbering; this table is
the reconciliation. All release gates (fmt / clippy -D warnings /
test --all-features) verified green on the formatted tree.

### OPEN — P0 blockers

| # | Finding | Evidence |
| - | ------- | -------- |
| 1 | SessionActor ownership/shutdown: `closing` copied by value in `Clone`; no shared inner; no cancellation for long jobs | `src/session/actor.rs:77` |
| 2 | One driving authority: `drive_pipeline` exists + scenario replay uses it, but no `DriveOrigin` provenance and ~25 direct `execute_act` call sites remain (exploration, conformance, audit drivers) | `src/execution/drive.rs`, callers |
| 3 | Intent: `Focus` plans a left click (`vocab.rs:350`); `EnsureFocus` uses a click as focus primitive so Activate/Toggle can double-fire (`plan.rs:82`); `Open` is `Safe` + Enter for every control (`vocab.rs:394`); `execute=true` still fuses plan+authorization (no `plan_id`/`max_risk` two-step) | `src/intent/{plan,vocab}.rs`, `src/mcp/params/intent.rs` |
| 4 | Lease completeness: release needs no token; restart checks run-ownership but not the human lease; `stop` checks neither; `tui_run close kill_sessions=true` kills leased sessions ungated. (Stale sub-claim: `probe_query_response` no longer writes to the PTY — it drains the parser engine-side, `portable_pty.rs:639`; the orchestrator comment saying otherwise is wrong.) | `src/session/lease.rs`, `handlers/session.rs:193,250`, `handlers/run.rs:164` |
| 5 | Scenario fail-fast: failed step increments and the loop continues; no `on_failure=stop`, no `skipped_due_to_prior_failure` | `src/scenario/runner.rs:88-360` |
| 6 | tmux honesty: every wait returns `WaitReason::ScreenChange` (`tmux.rs:586`); `input_families` lists `RawByte` while `raw_input:false`; `native_semantic:true` claimed for attach; no `ProcessOwnership` model; synthetic ProcessExited on detach | `src/backend/tmux.rs` |
| 7 | Native channel: `read_line` has no max-frame bound; channel file grows unbounded (offset advances, never truncated). (Landed half: bounded event ring + seq cursors + truncation detection.) | `src/semantic/native.rs:247` |
| 8 | Native adapters: `NativeNode.actions` parsed but consumed nowhere (dead data); Textual fallback id is `id(widget)` memory address; snippets emit snapshots only | `src/semantic/node.rs`, `src/framework/adapters.rs:114` |
| 9 | Evidence health: no `EvidenceHealth` model on DriveOutcome; frame-commit failures can still yield citable defaults | `src/execution/drive.rs` |

### OPEN — P1/P2

| # | Finding | Evidence |
| - | ------- | -------- |
| 12 | `ActorBusy` maps to `NoSession`; no `session_busy`/retryable category | `src/session/actor.rs:305` |
| 13 | Lifecycle mutation vs lease (subsumed by #4) | — |
| 16 | Native suffix match takes first hit, no uniqueness/ambiguity report | `src/semantic/native.rs:822` |
| 17 | Native focus rewrites headline + matched node but never clears stale `focused=true` on other controls | `src/semantic/native.rs:364-430` |
| 20 | Fake `AUDIT-METRICS` finding on clean runs; findings still carry passes/metrics | `src/audit/transaction.rs:170` |
| 21 | `rule_id` landed, but severity/category remain `String`; no occurrence_id/sorted-key fingerprints | `src/audit/mod.rs:139` |
| 22 | Audit uncertainty: no Unverified/LowConfidence/Ambiguous verdicts | drivers |
| 23 | `NextObservation` is prose (`suggestion`/`rationale`), no structured tool+arguments | `src/audit/repair.rs:128` |
| 25 | `action + dozens of Option<T>` request shapes (tagged unions) | `src/mcp/params/*` |
| 28 | Target resolution: `format!("{kind:?}")` wire slug (`vocab.rs:440`); comment-priority mismatch in nearest_candidates | `src/intent/vocab.rs` |
| 29 | Risk classification: label heuristics remain the authority; no native/contract risk attestation | `src/intent/vocab.rs` |
| 31 | Only the run manifest carries a schema tag; ledger/frame/event/finding/scenario/contract formats unversioned | `src/run/manifest.rs:61` |
| 32 | `reopen()` flips `closed=false`; no resume epochs | `src/run/persistence_impl.rs:458` |
| 33 | `tui_run new` silently replaces an ephemeral run holding evidence (no `discard=true` / count refusal) | `handlers/run.rs` New |
| 36-42 | Construction features: tui_inspect, multi-state scaffold, workflow object, regression-asset generation, semantic render diffs, framework diagnostic knowledge, adapter versioning | not started |
| 43 | Partially done (registry-generated SKILL sections); full single-descriptor doc generation open | registry |

### FIXED (verified in code)

- **26** BackendParam VARIANTS ↔ enum consistent; tmux parses (serde-typed enum; no FromStr drift surface)
- **30** Coverage delta stateless via caller-owned `since_seq` (`handlers/coverage.rs:86`)
- **34** Completion honesty: `MayBeSilent`/`OutputClosed`/`BudgetExpired` vocabulary; budget plumbed through every strategy
- **35** `require_wait` consults declared `supported_waits` for conditional families (`backend/mod.rs:210`)
- **44** `repair` marked as alias of `diagnose` in registry summary
- **10** (largely) probe timeline armed AT the stimulus (`diagnostic/mod.rs:179`); frame-history capture via transition capture
- **11** (partially) typed stale-state guards (executor validates atomically with send); remaining: internal stringly error surfaces
- **15** detection names OpenTUI/Ink/Bubble Tea but integration is tier-1 only — detection-side honesty landed, adapters open (see 36-42)

### Priority order (implementation sequence)

1→3→4→5→6→7→8→9→2 as P0, then 12/16/17/20/21/23, then construction set.

