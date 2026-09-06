# Changelog

All notable changes to `tui-lab` are documented here. The project has not
yet cut a public release; the `Unreleased` sections below carry the
changes that will land in the first tagged version.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/) once releases
begin.

## [Unreleased]

### Fixed — beta stability audit (P0)

This block resolves the invariant failures found by the public-beta
stability audit. Each entry cites the audit finding it fixes.

- **Durable evidence can no longer be reported as persisted when the write
  failed.** `promote`, `flush`, `reopen`, and `commit_frame` propagate
  open/write/serialization errors instead of swallowing them; the ledger
  cursor only advances past records that are actually on disk; `close`
  rolls back to an open run when the durable write fails; `reopen` returns
  `Result` and leaves the run closed at the previous epoch when the
  manifest update fails; evidence health distinguishes in-memory commits
  from durable commits. (Audit P0-1)
- **Restored-run damage is visible.** The inverted condition in
  `tui_run status` was fixed: a damaged restored run reports
  `restore.degraded = true` with its warnings inline, instead of `null`
  while a healthy run reported an empty health block. (Audit P0-2)
- **`tui_run resume` proves completion before mutating the target.** The
  candidate is restored and validated, ownership conflicts resolved, the
  current run flushed, and required sessions detached BEFORE the target's
  reopen/manifest commit; a refused resume leaves the target logically
  closed and bit-for-bit unchanged, and `resume_epoch` keeps its
  provenance meaning. A session whose stop failed is never unbound —
  resume aborts naming the failed session. (Audit P0-3)
- **`tui_session stop` can no longer bypass run ownership or the human
  lease.** Lifecycle authorization is separate from the driving gate: the
  actor is resolved regardless of run-open state, then owner → authority
  → live lease are checked before any mutation. A foreign run cannot stop
  a session; a leased session cannot be stopped even by its own run.
  Wire-level tests cover foreign+leased, closed+leased, expired-lease,
  and same-run cleanup paths. (Audit P0-4)
- **`SessionPool::stop` closes the teardown race and surfaces target
  errors.** The actor's `closing` state is asserted before target
  shutdown so in-flight enqueues are rejected; the target's stop result
  is preserved and MCP distinguishes `target_stop_failed` from actor
  teardown failure. (Audit P0-5)
- **In-flight run-switch provenance race closed.** Every authorized actor
  job captures the authorizing run's ticket and re-verifies it at
  evidence-commit time; if `tui_run new/resume` swaps the live run under
  an in-flight operation, the evidence is DROPPED with an error naming
  both runs — never committed to the wrong run. Deterministic race tests
  pin the behavior. (Audit P0-6)
- **One canonical machine-driving pipeline.** A `RunEvidenceSink`
  installed on the session by the authorization gate makes the full
  evidence pipeline (typed origin, citable frames, ledger row, event and
  coverage folds, redaction, persistence health) a property of the
  canonical executor for every driving origin — exploration, audits,
  intents, probes, replay, repro — instead of only `tui_act`. Stimulated
  probes persist real causal transactions. An origin-parity suite drives
  every origin through the wire and proves identical evidence contracts.
  (Audit P0-7)

### Fixed — beta stability audit (P1)

- **Intent `plan_id` binds the whole previewed plan**: session, verb,
  risk class, and step-shape hash are checked at execute time; previewed
  plans expire after 5 minutes; `execute=true` requires the plan_id
  returned by the preview, and a refused risk fence no longer consumes
  the one-shot plan. (Audit finding 8)
- **Recorded intents replay faithfully**: a first-class `intent` scenario
  step carries target + verb as semantic facts; replay re-resolves the
  target on the live screen and re-runs the focus-secured plan engine
  instead of replaying frozen keys a layout change breaks. Sensitive
  payloads in intent steps are redacted to `${NAME}` parameter references
  exactly like sensitive acts. (Audit finding 9)
- **Coverage sequence identity survives restore/resume**: the sequence
  high-water mark is reconstructed from the restored ledger, so
  post-resume events continue above every pre-close cursor instead of
  restarting at 0. (Audit finding 10)
- **Close/persist responses report fresh state**: `tui_run close`'s
  `final` summary is computed after the close; an already-persistent
  `tui_run persist` flushes and answers with the current durability
  picture instead of a stale echo. (Audit finding 11)
- **`deep_isolation` renamed `restart_between_mutations`** (old spelling
  still accepted): restarting the app cannot undo file writes or network
  requests, so the flag no longer implies mutation consent —
  `allow_mutation=true` is required alongside it. (Audit finding 13)
- **Workflow framework context cites its root provenance**: detection
  runs against the run's recorded launch cwd, not the server process's
  cwd; an explicit `cwd` is labeled an override; `workflow verify`'s
  lease gate follows each audit surface's own classification.
  (Audit finding 14)
- **Audit transactions are state-residue verification**: one policy for
  screen-structure change — contextual evidence at INFO, never a Warn
  defect, never silently dropped — with the semantics documented in
  place. (Audit finding 15)

### Changed

- God-object decomposition round 2: `Session` state split into cohesive
  subsystem holders (observation, events, recording, semantics);
  `PortablePtyBackend` split into emulator, process, wait-evaluator,
  input encoders, raw capture, event clock, protocol responder; MCP
  parameter/handler families separated; run storage extracted into
  `ArtifactStore`, `ScenarioStore`, `FindingStore`, `RunGraphs`,
  `CoverageState`, `ContractState`; the `tui_run` handler match split
  into per-action modules.

### Removed

- The empty `tui_test_backend` cargo feature: it gated no code and
  advertised an unsupported backend. If Microsoft tui-test stabilizes,
  the feature returns together with its implementation. (Audit finding 17)

### Added

- `LICENSE` (MIT text), this changelog, a declared and verified MSRV
  (`rust-version = "1.92"`), and crates.io metadata (`repository`,
  `readme`, keywords, categories). (Audit finding 17)
- An MSRV lane in CI. (Audit finding 17)
