---
name: god-object-review-2026-09-02
description: Public-beta review findings for tui-lab god objects and architectural coherence
metadata:
  type: project
---

# tui-lab Public Beta God-Object Review

## Status
Reviewed without edits. Verdict: not yet public-beta-stable. The engine is substantially real and useful, but architectural coherence needs work before adding more features.

## P0 Findings
1. **Architecture**: `src/mcp/tools.rs` (4,165 lines) and `src/run/mod.rs` (3,455 lines) are god objects
2. **Audit safety**: `is_active()` conflates "needs live session" with "drives TUI", blocking passive diagnostics under human lease
3. **Audit API**: Internal `lifecycle_exit` not selectable through MCP
4. **Agent actions**: Semantic intent system not exposed through MCP, activation plan can press Enter on wrong control
5. **Diagnostics**: `repair` should become diagnosis/evidence context
6. **Source attribution**: Coverage correlation can produce actionable source at exactly the confidence threshold
7. **API consistency**: Engine profile enum, MCP selector, registry, docs have drifted
8. **Resources**: `terminal-profile` resource exists but missing from discovery
9. **Coverage**: `delta` uses run-global cursor, single consumer can consume another's delta
10. **Params**: `src/mcp/params.rs` (1,909 lines) duplicates action schema

## Refactoring Targets (God Objects)

### 1. TuiLabServer (src/mcp/tools.rs — 4,165 lines)
Split into handler families:
- `src/mcp/handlers/session.rs` — tui_session
- `src/mcp/handlers/observe.rs` — tui_observe, tui_probe, tui_wait, tui_assert
- `src/mcp/handlers/interact.rs` — tui_act, tui_checkpoint
- `src/mcp/handlers/scenarios.rs` — tui_scenario, tui_record
- `src/mcp/handlers/exploration.rs` — tui_explore
- `src/mcp/handlers/audit.rs` — tui_audit, tui_explain
- `src/mcp/handlers/coverage.rs` — tui_coverage
- `src/mcp/handlers/framework.rs` — tui_framework
- `src/mcp/handlers/run.rs` — tui_run
- `src/mcp/handlers/contract.rs` — tui_contract
- `src/mcp/resources.rs` — resource routing

### 2. RunContext (src/run/mod.rs — 3,455 lines)
Compose from stores:
- `EvidenceLedger` — transactions, frame records, render ops
- `ArtifactStore` — artifact state
- `ScenarioStore` — scenario recording + saved scenarios
- `FindingStore` — findings, baselines
- `CoverageStore` — coverage + cursors
- `DesignState` — contracts, conformance
- `ExplorationState` — state graph, exploration data

### 3. Session (src/session/state.rs — 1,472 lines)
Compose internally:
- `ProcessState`
- `ObservationState`  
- `EventState`
- `SemanticState`
- `RecordingState`
- `NativeIntegrationState`
- `ControlState`

### 4. AuditProfile (src/audit/orchestrator.rs — 1,358 lines)
Single profile descriptor:
- `AuditProfileDescriptor` with id, public, execution_scope, input_behavior, mutation_risk, included_in_full, driver

### 5. Params (src/mcp/params.rs — 1,908 lines)
- Generate mirrored shapes from canonical source
- Split by handler family with re-exports

## P1 Findings (deferred to post-beta)
- Errors structured but remediation prose-only
- `anomalies` mislabeled
- Evidence not addressable as MCP resources  
- Session combines too many concerns
- Documentation drift
