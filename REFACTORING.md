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

(none — all nine fixed; see FIXED)

### OPEN — P1/P2

| # | Finding | Evidence |
| - | ------- | -------- |
| 13 | ~~Lifecycle mutation vs lease~~ FIXED with #4 (`b5cbe20`) | — |
| 16 | ~~Native suffix match takes first hit, no uniqueness/ambiguity report~~ FIXED with 17 (`ca9af5d`) | — |
| 17 | ~~Native focus rewrites headline + matched node but never clears stale `focused=true` on other controls~~ FIXED with 16 (`ca9af5d`) | — |
| 20 | ~~Fake `AUDIT-METRICS` finding on clean runs; findings still carry passes/metrics~~ FIXED (`a1349b7`) | — |
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

- **2** One driving authority with provenance: `DriveOrigin` enum (act/intent/scenario/explore/repro/explore_semantic/audit/conformance/probe) rides every transaction and ledger row; ALL production drivers migrated off the untagged path (exploration ×3, audit drivers ×4, conformance, diagnostic probe, scenario replay); tui_act/intent responses carry origin on the wire; exploration ledger test proves every row is `origin=explore` (`117fda3`)
- **9** Evidence health: `EvidenceHealth {frame_commits, ledger_recorded}` on DriveOutcome; failed frame commits are `null` + error in the frames response (never a fabricated `frame:0`), `evidence_health.healthy` + failures ride the tui_act/tui_intent responses and warnings; integration test proves open-run healthy vs closed-run honest (`c1a0163`)
- **8** Adapter dead data: `NativeNode.actions` consumed — declared verbs become affordances (new honest `Invocation::Declared` for verbs with no conventional invocation), replacing inference for that control on both overlay shapes; native-only insertions carry verbs too. Textual ids are structural paths (widget id / type+sibling-index under parent path), never `id(widget)`. Textual mixin declares per-widget verbs + emits focus events, not snapshots alone (`a2d180f`)
- **4** Lease completeness: release requires the `lease_id` token issued at acquire (wrong token → `control_leased` naming the holder; expiry needs none); restart, stop, and `tui_run close kill_sessions=true` all refuse live-leased sessions with holder + retry_after_ms (close checks via the pool — `with_sess` is run-closed-gated after close); fixed as `b5cbe20`, subsumes 13
- **7** Native channel bounds: `read_until` capped at 1 MiB per window (oversize skipped window-per-poll, resync at next newline, counted `frames_invalid`); non-UTF-8 refused not sticky; compaction rewrites the file to the unconsumed tail (atomic rename) past 8 MiB consumed history, offset restarts at 0 (`350b13c`)
- **1** SessionActor shared ownership: `Arc<SessionActorInner>` (shared `closing` + join slot); stop removes from directory first, acks, joins (`049c144`)
- **3** Intent semantics: focus moves are PROVEN FocusGraph Tab traversals (never clicks); AssertFocus guard between hops and payload; `Focus` Safe + payload-free; `Open` Mutating and MenuItem-only; plan/execute two-step contract (plan_id execute-once tickets + `max_risk` fence); E2E activation-ledger proves exactly-once (`33de5cf`)
- **5** Scenario fail-fast: `on_failure` (stop|continue, stop default) on the model + wire override; `skipped_due_to_prior_failure` marking, `steps_skipped` count, `status: stopped_on_failure`; a skipped step fails `passed` (`a30290c`)
- **6** tmux truth: per-condition WaitReasons (live-tmux conformance-pinned); `RawByte` dropped from input_families; `native_semantic:false` on attach; `ProcessOwnership` model (SpawnedChild/Attached) + `kill_on_stop()` trait method; restart ledger records Terminated only for owned processes (`2328666`)
- **12** `session_busy` retryable category (distinct from `no_session`) with structured `retry_after_ms` details; ActorDropped → backend_error (`09e618e`)
- **26** BackendParam VARIANTS ↔ enum consistent; tmux parses (serde-typed enum; no FromStr drift surface)
- **30** Coverage delta stateless via caller-owned `since_seq` (`handlers/coverage.rs:86`)
- **34** Completion honesty: `MayBeSilent`/`OutputClosed`/`BudgetExpired` vocabulary; budget plumbed through every strategy
- **35** `require_wait` consults declared `supported_waits` for conditional families (`backend/mod.rs:210`)
- **44** `repair` marked as alias of `diagnose` in registry summary
- **10** (largely) probe timeline armed AT the stimulus (`diagnostic/mod.rs:179`); frame-history capture via transition capture
- **11** (partially) typed stale-state guards (executor validates atomically with send); remaining: internal stringly error surfaces
- **15** detection names OpenTUI/Ink/Bubble Tea but integration is tier-1 only — detection-side honesty landed, adapters open (see 36-42)
- **16/17** Native resolution: relaxed matches (suffix/label) are uniqueness-gated — ambiguity is reported (`NativeOverlayReport.ambiguous`), never first-hit; focus is exclusive (`clear_tree_focus_except` on the tree, flat mirror follows `sem.focus.control_id`) (`ca9af5d`)
- **20** Audit timing is report metadata: `run_verified` returns `(findings, AuditMetrics)`; clean runs produce zero findings (no synthetic AUDIT-METRICS row, no stamping onto finding evidence); `ProfileReport.metrics` + `metrics` on the tui_audit response carry the numbers (`a1349b7`)

### Priority order (implementation sequence)

Remaining P0: none. Next: P1/P2 — 21 (typed severity/category) → 23 (structured NextObservation) → 28 (vocab slugs) → 31 (format versioning) → 32 (reopen epochs) → 33 (ephemeral evidence discard), then construction set 36-42.

