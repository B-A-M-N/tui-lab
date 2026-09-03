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

