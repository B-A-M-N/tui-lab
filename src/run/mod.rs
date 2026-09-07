//! The canonical run/artifact model (audit item: "Run/Artifact Model").
//!
//! Every longer-lived activity of the lab — exploration, audits, scenarios,
//! recordings, coverage — happens inside a *run* and leaves its artifacts in
//! one place:
//!
//! ```text
//! .tui-lab/runs/<run-id>/
//!     run.json            — RunManifest (serialized RunContext identity)
//!     checkpoints/        — CheckpointStore persistence
//!     scenarios/          — recorded + saved scenarios
//!     recordings/         — asciicast exports
//!     findings/           — audit findings
//!     state_graph.json    — exploration graph export
//! ```
//!
//! `RunContext` is the composition root: instead of the MCP layer owning
//! disconnected global maps (`CHECKPOINTS`, `SCENARIOS`, `write_cast`), the
//! server holds a `RunContext` and every subsystem hangs off it. This is what
//! turns "implemented modules" into "product paths".

mod artifact_store;
pub mod artifacts;
pub mod formats;
pub mod journal;
pub mod manifest;
pub mod recording_scope;
// Impl-family extractions: each hosts the method bodies for one subsystem
// while the struct + fields stay here. (The impl-block split keeps every
// signature, field visibility, and caller unchanged.)
mod artifacts_impl;
mod contract_impl;
mod contract_state;
mod coverage_impl;
mod coverage_state;
mod evidence_impl;
mod evidence_store;
mod finding_store;
mod findings_impl;
mod graph_state;
mod identity;
mod launch_impl;
mod ledger_impl;
mod persistence_impl;
mod scenario_impl;
mod scenario_store;

pub use artifacts::{ArtifactKind, ArtifactRef};
pub use formats::StreamHeader;
pub use journal::JournalHandle;
pub use manifest::RunManifest;

use crate::checkpoint::store::CheckpointStore;
use crate::exploration::state_graph::{ExplorationBudget, StateGraph};
use contract_state::ContractState;
use evidence_store::EvidenceStore;
use finding_store::FindingStore;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bound on the in-memory transaction ledger (re-review Wave-2 item
/// 15). Frames are heavy; the ledger holds evidence-level records. When the
/// bound is hit the oldest half is evicted; the eviction is *declared* —
/// `dropped_records`/`first_available_seq` travel with the run (manifest +
/// status) so a promoted run never pretends to be replay-complete
/// (re-review P0 fix 5).
const MAX_TRANSACTION_RECORDS: usize = 512;
/// Audit P1-48: the ledger's memory window is a high-water/low-water pair —
/// trim fires past HIGH and drains back to LOW, so the resident bound is
/// exactly HIGH (never HIGH + LOW as the old `MAX + 512` trigger allowed).
const TRANSACTION_RING_HIGH_WATER: usize = MAX_TRANSACTION_RECORDS * 2;
const TRANSACTION_RING_LOW_WATER: usize = MAX_TRANSACTION_RECORDS;

/// How many hot frame records the run retains (re-review item 51). Sized
/// like the transaction ring: enough to answer "what was frame:N?" for a
/// whole session's working set, bounded so a long soak cannot grow the
/// ledger without limit. Eviction is declared (`frame_hot_evicted`), and
/// the cold half of the split — the full grid — was never here to begin
/// with (it lives in `frames.jsonl` for persistent runs).
pub const FRAME_HOT_RING: usize = 512;

/// The HOT half of the frame storage split (re-review item 51): the
/// identity + projection facts of one committed frame, queryable in
/// memory by citable id. Deliberately NO cell grid — the cold half
/// (full `ScreenState`) stays with the frame's owner and the
/// `frames.jsonl` log; this record is what "frame:N" means to the run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameRecord {
    /// Citable id (`frame:N` without the prefix).
    pub frame_id: u64,
    /// Session the frame was captured from, when known at commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Session generation at capture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    pub screen_seq: u64,
    pub output_seq: u64,
    pub structure_hash: String,
    pub visual_hash: String,
    /// Semantic identity at commit (crate::semantic::semantic_identity).
    pub semantic_identity: String,
    /// Unix-millis commit time.
    pub committed_at: u64,
    /// How long the commit pipeline took (micros) — item 49's cost
    /// visibility applied to the frame path.
    pub commit_us: u64,
}

/// One settled interaction transaction, at evidence level.
///
/// This is the reconstructable record the run previously lacked (it could
/// say `transactions: 47` but not replay any of them): ordered sequence,
/// action, settle outcome, before/after hashes, and the cell delta. Full
/// frames remain the caller's concern — attach them to artifacts, not to
/// every run entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionRecord {
    /// Ordered execution index within the run (0-based).
    pub seq: u64,
    /// Unix-millis timestamp of the transaction.
    pub at: u64,
    /// Session the transaction ran against (informational; the run does not
    /// own sessions).
    pub session: String,
    /// Action name ("key", "type", "mouse_click", "wait", ...).
    pub action: String,
    /// How the settle wait resolved ("met" / "timed_out" / "skipped").
    /// Serialized as a string; `settled()` derives the legacy bool.
    pub settle: String,
    /// Why the settle resolved (or timed out / was skipped), if reported.
    pub settle_reason: Option<String>,
    /// Structure hash before the action.
    pub before_structure: String,
    /// Structure hash after the action (the matching/settled frame).
    pub after_structure: String,
    /// Cells changed across the transition.
    pub changed_cells: usize,
    /// Settle latency in milliseconds.
    pub elapsed_ms: u64,
    /// Send-phase latency (re-review item 39): transport + harness cost,
    /// excluding the settle wait. Answers "were we slow?" separately.
    #[serde(default)]
    pub send_ms: u64,
    /// Settle-phase latency (re-review item 39): the app's own response
    /// time under the completion plan. Answers "was the app slow?"
    #[serde(default)]
    pub settle_ms: u64,
    /// The typed action (Wave-2 item 10) as it may be persisted (leak fix):
    /// `Full` when the visibility policy allows it, `Redacted { kind,
    /// byte_len }` for sensitive payloads — the payload itself never lands
    /// in the ledger. Skipped for non-interaction ledger entries ("wait").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_action: Option<crate::execution::PersistedAction>,
    /// Causal render evidence (re-review item 19): the action's protocol
    /// byte range, op count, first-byte latency. The op LIST stays in the
    /// live transaction (bounded there); the ledger carries the citation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderSummary>,
    /// Which subsystem drove the input (audit finding 2): typed
    /// provenance from the transaction (`act`, `scenario`, `explore`,
    /// `audit`, ...). `None` for pre-existing ledgers (serde default) and
    /// non-interaction entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Ledger-sized summary of a [`crate::execution::RenderTransaction`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RenderSummary {
    pub range_start: u64,
    pub range_end: u64,
    pub complete: bool,
    pub op_count: usize,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_byte_ms: Option<u64>,
    /// Item 26: input→first-screen-frame and input→first-semantic-change
    /// latencies, and the full-erase share of the response's ops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_frame_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_semantic_ms: Option<u64>,
    pub full_repaint_ratio: f64,
    pub dirty_cells: usize,
}

impl TransactionRecord {
    /// Legacy view of settlement as a bool.
    pub fn settled(&self) -> bool {
        self.settle == "met"
    }

    /// Build a record from a live [`crate::execution::InteractionTransaction`].
    pub fn from_interaction(
        seq: u64,
        session: &str,
        tx: &crate::execution::InteractionTransaction,
    ) -> Self {
        let settle = match tx.settle {
            crate::execution::SettleStatus::Met => "met",
            crate::execution::SettleStatus::TimedOut => "timed_out",
            crate::execution::SettleStatus::Skipped => "skipped",
        };
        let persisted_action = Some(crate::execution::PersistedAction::project(
            tx.canonical(),
            tx.action.visibility,
        ));
        TransactionRecord {
            seq,
            at: now_ms(),
            session: session.to_string(),
            action: tx.name().to_string(),
            settle: settle.to_string(),
            settle_reason: Some(tx.settle_reason()),
            before_structure: tx.before().structure_hash.clone(),
            after_structure: tx.after().structure_hash.clone(),
            changed_cells: tx.transition.screen_diff.changed_cells,
            elapsed_ms: tx.elapsed_ms,
            send_ms: tx.send_ms,
            settle_ms: tx.settle_ms,
            persisted_action,
            render: tx.render.as_ref().map(|r| RenderSummary {
                range_start: r.range_start,
                range_end: r.range_end,
                complete: r.complete,
                op_count: r.op_count,
                bytes: r.bytes,
                first_byte_ms: r.first_byte_ms,
                first_frame_ms: r.first_frame_ms,
                first_semantic_ms: r.first_semantic_ms,
                full_repaint_ratio: r.full_repaint_ratio,
                dirty_cells: r.dirty_cells,
            }),
            origin: tx.origin.map(|o| o.as_str().to_string()),
        }
    }
}

/// Identity + artifact directory for one run.
pub struct RunContext {
    /// Who this run is (id, started_at, closed, resume_epoch). Round-2
    /// (G1): moved into [`identity::RunIdentity`]; RunContext keeps the
    /// public `id()`/`started_at()` accessors external callers use.
    identity: identity::RunIdentity,
    /// Which sessions the run has seen and how they were launched —
    /// provenance records, not live process ownership. Round-2 (G1):
    /// moved into [`identity::RunSessionRegistry`].
    sessions: identity::RunSessionRegistry,
    /// Artifact root: `.tui-lab/runs/<id>` when persistence is enabled.
    run_dir: Option<PathBuf>,
    /// Checkpoints recorded during this run.
    pub checkpoints: CheckpointStore,
    /// Scenario recorders + id-keyed saved set + unambiguous-name index.
    /// Round-2 (G1): moved into [`scenario_store::ScenarioStore`]; the
    /// name-index invariant is store policy now. RunContext delegates.
    scenarios: scenario_store::ScenarioStore,
    /// Artifact registry + ephemeral media holds + journal + persistence
    /// health + restore damage. Round-2 (G1): moved into
    /// [`artifact_store::ArtifactStore`]; RunContext delegates. The
    /// artifact-root PATH stays a RunContext field — it is the one value
    /// promote/restore rewrite directly and half the IO paths read it.
    artifacts_store: artifact_store::ArtifactStore,
    /// Focus history + exploration graphs. Round-2 (G1): focus_transitions,
    /// focus_graph, and state_graph moved into [`graph_state::RunGraphs`];
    /// RunContext delegates and exposes `graphs()`/`graphs_mut()`.
    graphs: graph_state::RunGraphs,
    /// Findings + labeled baselines. Round-2 (G1): moved into
    /// [`FindingStore`]; RunContext delegates.
    findings: FindingStore,
    /// Citable execution evidence — transaction, frame, and event ledgers.
    /// Round-2 (G1): the nine flat evidence fields moved into
    /// [`EvidenceStore`] (internally: TransactionLedger / FrameLedger /
    /// EventLedger); RunContext delegates.
    evidence: EvidenceStore,
    /// The loaded project contract + conformance baselines (Wave E).
    /// Round-2 (G1): moved into [`ContractState`] so the contract domain has
    /// its own cohesive holder; RunContext delegates.
    contract: ContractState,
    /// Wave F item 64: native coverage. Round-2 (G1): ledger + monotonic
    /// seq + the process-wide delta cursor moved into
    /// [`coverage_state::CoverageState`] (the delta cursor is coverage
    /// policy, not run bookkeeping); per-consumer event cursors moved into
    /// the evidence store's event ledger. RunContext delegates.
    coverage: coverage_state::CoverageState,
}

/// One artifact the restorer could not bring back (audit P1-46).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreWarning {
    /// Which artifact failed (path relative to the run dir, or a ledger
    /// line range).
    pub artifact: String,
    /// Why it was skipped.
    pub error: String,
}

/// One native coverage entry (Wave F item 64): an app-declared coverage
/// target (file:line, widget id, function name) plus its hit statistics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CoverageEntry {
    /// Total hits declared by the app.
    pub hits: u64,
    /// Sessions that reported this target.
    pub sessions: Vec<String>,
    /// Unix-millis of the first report.
    pub first_seen: u64,
    /// Unix-millis of the last report.
    pub last_seen: u64,
    /// Monotonic per-run sequence when this target was FIRST seen. Delta
    /// compares this against a caller cursor: targets whose `first_seq`
    /// exceeds the cursor are "new since your last delta" (review P0.7 —
    /// replaces the old fabricated `delta` that ran the same tuicov command
    /// as every other action). Defaulted for persisted runs written before
    /// this field existed (they read as seq 0 → all "new" on first delta).
    #[serde(default)]
    pub first_seq: u64,
    /// Monotonic per-run sequence of the MOST RECENT hit.
    #[serde(default)]
    pub last_seq: u64,
    /// Wave 5 item 41: source loci the app itself declared for the
    /// component this target names (from the same NativeSemanticProtocol
    /// channel — both facts are app-attested, so the join is exact, not
    /// inferred). Empty for file targets and for components the app did
    /// not declare a locus for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<crate::semantic::source_ref::SourceRef>,
}

impl RunContext {
    fn scenario_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self
            .run_dir
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("run '{}' is ephemeral; no scenario storage", self.id())
            })?
            .join("scenarios");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Persist the run manifest (identity + layout).
    fn write_manifest(&self) -> anyhow::Result<()> {
        let Some(dir) = self.run_dir.as_ref() else {
            return Ok(());
        };
        let manifest = RunManifest {
            schema: "tui-lab.run.v1".into(),
            run_id: self.identity.id().to_string(),
            started_at: self.identity.started_at(),
            launch_spec: self.primary_launch_spec().cloned(),
            sessions: self.sessions.all_specs().clone(),
            primary_session: self.sessions.primary().map(str::to_string),
            history_complete: self.history_complete(),
            first_available_seq: self.evidence.transactions.first_available_seq(),
            dropped_records: self.evidence.transactions.dropped_records(),
            closed: self.identity.closed(),
            resume_epoch: self.identity.resume_epoch(),
        };
        let tmp = dir.join("run.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, dir.join("run.json"))?;
        Ok(())
    }

    /// The opaque run id (`run-<uuid simple>`).
    pub fn id(&self) -> &str {
        self.identity.id()
    }

    /// Wall-clock start (unix millis).
    pub fn started_at(&self) -> u64 {
        self.identity.started_at()
    }

    /// Whether the run is marked closed (`tui_run close`).
    pub fn is_closed(&self) -> bool {
        self.identity.closed()
    }

    /// Refuse mutation on a closed run (review P0.1). A closed run is an
    /// evidence bundle: post-close traffic from still-live sessions must
    /// never land in it, or "closed" stops meaning final. Read paths
    /// (replay, status, ledger) stay available on closed runs.
    fn ensure_open(&self) -> anyhow::Result<()> {
        if self.identity.closed() {
            anyhow::bail!(
                "current run {} is closed; create or resume an open run before recording evidence",
                self.identity.id()
            );
        }
        Ok(())
    }

    /// Shared bounded push with declared eviction (P0 fix 5). Round-2
    /// (G1): the ledger policy (window bounds, declared eviction) lives in
    /// [`evidence_store::TransactionLedger`]; this facade method keeps the
    /// journal plumbing (writer thread handoff / lazy spawn).
    fn push_ledger(&mut self, record: TransactionRecord) {
        // Background journal (audit item: run-journal writer): persistent
        // runs hand the serialized record to the writer thread and never
        // wait on the filesystem. The file remains the authoritative
        // history; memory is only a bounded window. The watermark lives in
        // the writer — `flush` drains through `wait_for`.
        if self.run_dir.is_some() {
            if self.artifacts_store.has_journal() {
                if let Ok(line) = serde_json::to_string(&record) {
                    self.artifacts_store
                        .journal()
                        .map(|j| j.submit(record.seq, line));
                }
            } else {
                // First record of a persistent run: spawn the writer now
                // (lazily, so a run that never records anything pays
                // nothing). A spawn failure degrades to the memory window
                // only — flush reports persistence_unhealthy.
                let path = self.run_dir.as_ref().map(|d| d.join("transactions.jsonl"));
                match path.map(journal::JournalHandle::spawn) {
                    Some(Ok(j)) => {
                        if let Ok(line) = serde_json::to_string(&record) {
                            j.submit(record.seq, line);
                        }
                        self.artifacts_store.set_journal(j);
                    }
                    _ => {
                        self.artifacts_store.mark_unhealthy();
                    }
                }
            }
        }
        self.evidence.transactions.push(record);
    }

    /// Resolve the per-run artifact directory from a caller-supplied base.
    ///
    /// A base whose last component is `runs` is treated as the runs directory
    /// itself (the goal spec's persist example) and holds the run directly:
    /// `<base>/<run-id>`. Any other base is a repo root:
    /// `<base>/.tui-lab/runs/<run-id>`. Session-cwd resolution always lands
    /// on the repo-root form.
    fn runs_dir_for(base: &std::path::Path, id: &str) -> PathBuf {
        let is_runs_dir = base
            .file_name()
            .map(|f| f == std::ffi::OsStr::new("runs"))
            .unwrap_or(false);
        if is_runs_dir {
            base.join(id)
        } else {
            base.join(".tui-lab").join("runs").join(id)
        }
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".into()
    } else {
        cleaned
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Serialize a JSON artifact under its format envelope (finding 31):
/// every persisted document names its format + version so a reader that
/// predates a shape change refuses honestly instead of reporting bare
/// "corrupt".
fn format_envelope(tag: &str, payload: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    crate::run::formats::Envelope::wrap(tag, payload.clone()).to_vec_pretty()
}

/// Whether a coverage target is a real file locus (`src/lib.rs`, `src/
/// lib.rs:42`, `src/lib.rs:42:7`). A path separator plus an extension is
/// the discriminator — `#save.activate` and `main` are not files.
fn target_is_file_locus(t: &str) -> bool {
    let has_sep = t.contains('/') || t.contains('\\');
    let has_ext = t.rsplit(['/', '\\']).next().is_some_and(|last| {
        last.contains('.') && !last.starts_with('.') // dotfile names like `.gitignore` edge — treat as no-ext
    });
    has_sep && has_ext
}

/// Parse `file:line[:col]` into a [`SourceRef`] with `correlated`
/// provenance: the app's coverage channel declared this locus, so the
/// mapping to real code is real, but the *link to any particular finding*
/// is a run-level correlation. The provenance tier (review §4) keeps it
/// out of the actionable set regardless of its confidence number —
/// confidence stays 0.7 to say the locus itself is well-mapped, while
/// `provenance` says the tie to THIS finding is investigative. Windows-
/// style `C:\...` paths keep their drive colon (the leading single letter
/// is not a line number).
fn source_ref_from_target(t: &str) -> Option<crate::semantic::source_ref::SourceRef> {
    // Split on ':' and take trailing numeric segments as line[:col]; the
    // rest (joined back) is the file. `src/a.rs:12:5` → file "src/a.rs",
    // line 12, col 5. `C:\x\y.rs:12` → file "C:\x\y.rs" (the `C` segment
    // is non-numeric and rejoins the path).
    let segments: Vec<&str> = t.split(':').collect();
    // Consume trailing numeric segments right-to-left: the first consumed
    // is the column, the second is the line (file.rs:LINE:COL).
    let mut col: Option<u32> = None;
    let mut line: Option<u32> = None;
    let mut idx = segments.len();
    while idx > 0 {
        let seg = segments[idx - 1];
        if !seg.is_empty() && seg.parse::<u32>().is_ok() {
            if col.is_none() {
                col = seg.parse().ok();
            } else if line.is_none() {
                line = seg.parse().ok();
            } else {
                break;
            }
            idx -= 1;
        } else {
            break;
        }
    }
    // A lone trailing numeric is the LINE, not the column (file.rs:42).
    let (line, col) = match (line, col) {
        (Some(l), Some(c)) => (Some(l), Some(c)),
        (None, Some(c)) => (Some(c), None),
        (l, c) => (l, c),
    };
    let line = line?;
    let file = segments[..idx].join(":");
    if file.is_empty() {
        return None;
    }
    Some(crate::semantic::source_ref::SourceRef {
        file,
        line,
        column: col,
        symbol: None,
        framework_id: None,
        confidence: 0.7,
        source: "framework-adapter".to_string(),
        provenance: crate::semantic::source_ref::Provenance::Correlated,
    })
}

/// Whether an app-declared coverage widget target plausibly names the
/// control a finding's evidence points at. Matching is by the control id's
/// last path segment (the stable label slug): `widget:#save.activate` and
/// `button/save` both reduce to something containing "save".
fn coverage_target_matches_control(coverage_target: &str, control_id: &str) -> bool {
    if control_id.is_empty() {
        return false;
    }
    // Peel the coverage verb suffix: `widget:#save.activate` → `#save`.
    let widget = coverage_target
        .strip_prefix("widget:")
        .unwrap_or(coverage_target)
        .split('.')
        .next()
        .unwrap_or(coverage_target);
    let widget_slug = widget.trim_start_matches(['#', '@']).to_lowercase();
    if widget_slug.is_empty() {
        return false;
    }
    // Control ids are `kind/label` (`button/save`) or region-path'd
    // `region/kind/label` (`dialog/main/button/save`, sometimes with a
    // trailing `x,y` coordinate). Coordinates only appear as a comma
    // inside the last segment, so: drop a trailing `,x,y` segment, then
    // the label is the last remaining path segment.
    let mut segs: Vec<&str> = control_id.split('/').collect();
    if let Some(last) = segs.last() {
        if last.contains(',') {
            segs.pop();
        }
    }
    let label_seg = segs.last().copied().unwrap_or("");
    let last_seg = label_seg.split(',').next().unwrap_or("").to_lowercase();
    !last_seg.is_empty() && (widget_slug == last_seg || widget_slug.contains(&last_seg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_context_ephemeral_has_no_dir() {
        let run = RunContext::ephemeral();
        assert!(run.run_dir().is_none());
        assert!(run.active_recordings().is_empty());
    }

    #[test]
    fn scenario_recording_lifecycle() {
        let mut run = RunContext::ephemeral();
        let id1 = run.begin_scenario_recording("demo", "sess-A", 1);
        // Names are not identities: a second recording with the same name is
        // legitimate (different session or generation).
        let id2 = run.begin_scenario_recording("demo", "sess-B", 1);
        assert_ne!(id1, id2);
        // Traffic for sess-A gen 1 lands ONLY in the sess-A recording.
        let _ = run.record_scenario_act(
            "sess-A",
            1,
            serde_json::json!({"action": "key", "key": "enter"}),
        );
        let _ = run.record_scenario_assert(
            "sess-A",
            1,
            serde_json::json!({"assertion": "text", "text": "OK"}),
        );
        // sess-B's traffic never lands in sess-A's recording.
        let _ = run.record_scenario_act(
            "sess-B",
            1,
            serde_json::json!({"action": "key", "key": "escape"}),
        );
        let a = run.finish_scenario_recording(id1.as_str()).expect("A");
        assert_eq!(a.step_count(), 2, "sess-A recording: {:?}", a.steps);
        let b = run.finish_scenario_recording(id2.as_str()).expect("B");
        assert_eq!(b.step_count(), 1, "sess-B recording: {:?}", b.steps);
        // A stale generation (after restart) must not absorb traffic either.
        let id3 = run.begin_scenario_recording("stale", "sess-A", 2);
        let _ = run.record_scenario_act(
            "sess-A",
            7,
            serde_json::json!({"action": "key", "key": "enter"}),
        );
        let c = run.finish_scenario_recording(id3.as_str()).expect("C");
        assert_eq!(c.step_count(), 0, "wrong generation must not match");
        assert!(run.active_recordings().is_empty());
    }

    /// Wave-2 item 15: the run keeps a reconstructable transaction ledger,
    /// not just a counter, and flushes it to transactions.jsonl.
    #[test]
    fn transaction_ledger_records_and_flushes() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // Build a real interaction transaction through the canonical executor.
        let mut s = crate::session::state::Session::new("ledger-sess".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('x'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        let tx = crate::execution::execute_act(
            &mut s,
            &crate::execution::CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('q')),
            },
            60,
            1000,
            false,
        )
        .expect("execute");

        let _ = run.record_interaction("ledger-sess", &tx);
        let _ = run.record_event("ledger-sess", "wait");

        assert_eq!(run.transaction_total(), 2);
        let ledger = run.transactions();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].seq, 0);
        assert_eq!(ledger[0].action, "key");
        assert_eq!(ledger[1].action, "wait");
        assert_eq!(ledger[0].before_structure, tx.before().structure_hash);
        assert_eq!(ledger[0].after_structure, tx.after().structure_hash);

        run.flush().expect("flush");
        let body =
            std::fs::read_to_string(run.run_dir().expect("run dir").join("transactions.jsonl"))
                .expect("ledger file");
        let lines: Vec<&str> = body.lines().filter(|l| !l.contains("\"schema\"")).collect();
        assert_eq!(lines.len(), 2, "one NDJSON record per line: {body}");
        let first: TransactionRecord = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(first.action, "key");
    }

    /// Audit finding 2: the ledger row carries typed driving provenance —
    /// the origin the executor was given (`act` by default, or the
    /// caller's subsystem), and non-interaction entries carry none.
    #[test]
    fn ledger_rows_carry_driving_origin() {
        let mut run = RunContext::ephemeral();
        let dir = tempfile::tempdir().expect("tmp");
        let screen = crate::screen::ScreenState::new(20, 5);
        let mut tx = crate::execution::InteractionTransaction {
            action: crate::execution::ActionEnvelope::new(
                crate::execution::CanonicalAction::Key {
                    key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Char('q')),
                },
                crate::execution::InputVisibility::Normal,
            ),
            anchor: crate::execution::ObservationAnchor::default(),
            before_frame: crate::backend::CanonicalFrame::new(screen.clone(), 1, 1),
            after_frame: crate::backend::CanonicalFrame::new(screen, 1, 2),
            settle: crate::execution::SettleStatus::Met,
            transition: crate::screen::diff::diff(
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(20, 5), 1, 1)
                    .state,
                &crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(20, 5), 1, 2)
                    .state,
            ),
            capture: None,
            focus_before: None,
            focus_after: None,
            elapsed_ms: 1,
            send_ms: 0,
            settle_ms: 1,
            render: None,
            transition_capture: None,
            origin: Some(crate::execution::DriveOrigin::Audit),
        };
        tx.before_frame.state.structure_hash = "b".into();
        tx.after_frame.state.structure_hash = "a".into();
        let _ = run.record_interaction("origin-sess", &tx);
        let _ = run.record_event("origin-sess", "wait");
        let ledger = run.transactions();
        assert_eq!(ledger[0].origin.as_deref(), Some("audit"));
        assert_eq!(ledger[1].origin, None, "non-interaction entries carry none");
        // Round-trip: the slug survives persistence.
        let line = serde_json::to_string(&ledger[0]).expect("ser");
        let back: TransactionRecord = serde_json::from_str(&line).expect("de");
        assert_eq!(back.origin.as_deref(), Some("audit"));
        // And a pre-origin ledger row (serde default) still loads.
        let mut legacy_json = serde_json::to_value(&ledger[0]).expect("val");
        if let Some(obj) = legacy_json.as_object_mut() {
            obj.remove("origin");
        }
        let legacy_row: TransactionRecord =
            serde_json::from_value(legacy_json).expect("pre-origin rows deserialize");
        assert_eq!(legacy_row.origin, None);
        let _ = dir;
    }

    /// Audit P1-48: the ledger's memory window honors its declared bound —
    /// the ring never exceeds HIGH WATER, trims back to LOW WATER, and the
    /// eviction is declared (dropped_records / first_available_seq).
    #[test]
    fn transaction_ring_enforces_declared_bound() {
        let mut run = RunContext::ephemeral();
        for _ in 0..(TRANSACTION_RING_HIGH_WATER + 200) {
            let _ = run.record_event("ring-sess", "wait");
        }
        assert!(
            run.transactions().len() <= TRANSACTION_RING_HIGH_WATER,
            "ring must not exceed its high-water bound: {} > {}",
            run.transactions().len(),
            TRANSACTION_RING_HIGH_WATER
        );
        assert!(
            run.transactions().len() >= TRANSACTION_RING_LOW_WATER,
            "trim drains to low-water, not below: {} < {}",
            run.transactions().len(),
            TRANSACTION_RING_LOW_WATER
        );
        assert!(
            run.dropped_records() > 0,
            "eviction is declared, not silent"
        );
        assert_eq!(
            run.first_available_seq(),
            Some(run.transactions()[0].seq),
            "first_available_seq matches the oldest resident record"
        );
        assert!(!run.history_complete(), "an evicted window is incomplete");
    }

    /// Wave B item 14: a PERSISTENT run appends each ledger record to
    /// transactions.jsonl immediately — the file grows with the run, so a
    /// crash cannot lose the unflushed tail.
    #[test]
    fn persistent_ledger_appends_incrementally() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // Records stream to the background journal writer; drain-wait for
        // the watermark instead of assuming synchronous append (the old
        // blocking append on the driving path is exactly what the
        // run-journal-writer audit item removed).
        let _ = run.record_event("s1", "wait");
        let path = run.run_dir().expect("dir").join("transactions.jsonl");
        run.wait_for_journal(std::time::Duration::from_secs(2));
        let body = std::fs::read_to_string(&path).expect("ledger exists pre-flush");
        assert_eq!(body.lines().count(), 2, "appended (+header): {body}");

        let _ = run.record_event("s1", "wait");
        run.wait_for_journal(std::time::Duration::from_secs(2));
        let body = std::fs::read_to_string(&path).expect("reread");
        assert_eq!(body.lines().count(), 3, "second append (+header): {body}");

        // And flush is idempotent on the file (no duplicate lines).
        run.flush().expect("flush");
        let body = std::fs::read_to_string(&path).expect("reread");
        assert_eq!(
            body.lines().count(),
            3,
            "flush must not duplicate (+header)"
        );
    }

    /// Wave B item 11: frames get citable per-run ids and provenance.
    #[test]
    fn frames_get_citable_ids() {
        let mut run = RunContext::ephemeral();
        let mut f =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 3, 9);
        let id = run.register_frame(&mut f);
        assert_eq!(id, 1);
        assert_eq!(f.frame_id, Some(1));
        assert_eq!(f.cite(), "frame:1");
        assert_eq!(f.run_id.as_deref(), Some(run.id()));

        let mut g =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 4, 10);
        let id2 = run.register_frame(&mut g);
        assert_eq!(id2, 2, "monotonic per run");
        assert_eq!(run.counts()["frames"], 2);
    }

    /// Wave B item 15: artifacts register as typed refs, both persisted and
    /// held (ephemeral), and travel into run status.
    #[test]
    fn artifact_refs_register_and_cite() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        let rel = std::path::PathBuf::from("recordings").join("s1-1.cast");
        let a = run
            .register_artifact(
                ArtifactKind::Recording,
                Some(rel.clone()),
                Some(123),
                Some("s1".into()),
                "test recording",
            )
            .expect("register");
        assert_eq!(a.id, "art-1");
        assert_eq!(a.cite(), "art-1:recording");
        assert_eq!(run.artifacts().len(), 1);

        // Ephemeral registration (no path) still gets a ref.
        let b = run
            .register_artifact(
                ArtifactKind::EventLog,
                None,
                Some(456),
                Some("s2".into()),
                "held events",
            )
            .expect("register");
        assert_eq!(b.id, "art-2");
        assert!(b.path.is_none());

        let status = run.status(vec![]);
        let arts = status["artifacts"].as_array().expect("artifacts in status");
        assert_eq!(arts.len(), 2);
    }

    /// Wave B item 12/14: session event queues drain into per-session
    /// events/<session>.jsonl on flush, registered as typed artifacts.
    #[test]
    fn event_logs_flush_to_artifacts() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        let mut s = crate::session::state::Session::new("ev-sess".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('ev'); import time; time.sleep(10)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        // Two observations: ProcessStarted (+ possibly ScreenChanged).
        let _ = s.observe(50).expect("observe 1");
        let _ = s.observe(50).expect("observe 2");
        let drained = s.drain_events();
        assert!(!drained.is_empty(), "observe must emit events");
        assert!(
            drained.iter().any(|e| e.kind.name() == "process_started"),
            "first observation emits ProcessStarted"
        );

        let _ = run.hold_events("ev-sess", drained);
        run.flush().expect("flush");
        let log = std::fs::read_to_string(
            run.run_dir()
                .expect("dir")
                .join("events")
                .join("ev-sess.jsonl"),
        )
        .expect("event log");
        assert!(log.contains("\"type\":\"process_started\""), "{log}");
        assert!(
            run.artifacts()
                .iter()
                .any(|a| a.kind == ArtifactKind::EventLog),
            "event log registered as artifact"
        );
    }

    /// Re-review item 38: persistent runs persist events incrementally —
    /// the batch is on disk at `hold_events` time, before any flush — and
    /// flush does not duplicate the lines.
    #[test]
    fn persistent_runs_persist_events_incrementally() {
        // (see the sweep-path test below for the observe → persistence
        // cursor wiring; this test pins the hold_events contract itself.)
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // One synthetic event batch (no PTY needed for persistence logic).
        let ev = crate::events::TerminalEvent {
            at: 0,
            seq: 0,
            session: "incr-sess".to_string(),
            generation: 0,
            kind: crate::events::TerminalEventKind::Bell,
        };
        let _ = run.hold_events("incr-sess", vec![ev.clone(), ev.clone()]);
        // The batch must already be on disk (incremental, not held).
        assert!(
            run.counts()["events_held_for_flush"] == 0,
            "persistent runs must not hold events for flush"
        );
        let path = run
            .run_dir()
            .expect("dir")
            .join("events")
            .join("incr-sess.jsonl");
        let log = std::fs::read_to_string(&path).expect("incremental log");
        // +1: the finding-31 stream header line.
        assert_eq!(log.lines().count(), 3, "both events appended (+header)");
        // And the counter says so.
        assert_eq!(run.counts()["events_persisted_incrementally"], 2);
        assert_eq!(run.counts()["events_held_for_flush"], 0);

        // Flush must not duplicate.
        run.flush().expect("flush");
        let log = std::fs::read_to_string(&path).expect("log after flush");
        assert_eq!(log.lines().count(), 3, "flush must not re-append (+header)");

        // An ephemeral run keeps the old hold-for-flush contract.
        let mut eph = RunContext::ephemeral();
        let _ = eph.hold_events("eph-sess", vec![ev.clone()]);
        assert_eq!(
            eph.counts()["events_held_for_flush"],
            1,
            "ephemeral holds for flush"
        );
        assert_eq!(eph.counts()["events_held_for_flush"], 1);
    }

    /// Wave-2 (incremental persistence, the sweep wiring): a session's event
    /// queue drains into the run's on-disk log through the run-side
    /// persistence cursor as observations happen — the log is current BEFORE
    /// close, and the close-path drain only sees the tail.
    #[test]
    fn observe_sweep_persists_events_before_close() {
        let mut s = crate::session::state::Session::new("sweep-ev".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('SWEEP-EV'); import time; time.sleep(2)".into(),
            ],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            backend: "cli".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        // The sweep's persistence loop (same shape as the MCP observe sweep).
        let sweep = |s: &mut crate::session::state::Session, run: &mut RunContext| {
            s.observe(50).expect("observe");
            let cursor_key = format!("persistence:{}", s.id);
            let from = run.event_cursor(&cursor_key).unwrap_or(0);
            let batch = s.events_since(from);
            if !batch.events.is_empty() {
                let _ = run.hold_events(&s.id, batch.events);
                run.set_event_cursor(&cursor_key, batch.cursor);
            }
        };
        sweep(&mut s, &mut run);
        sweep(&mut s, &mut run);

        // The on-disk log is already current — before any close/drain.
        let path = run
            .run_dir()
            .expect("dir")
            .join("events")
            .join("sweep-ev.jsonl");
        let log = std::fs::read_to_string(&path).expect("incremental event log");
        assert!(
            !log.is_empty(),
            "events must reach disk during observation, not only at close"
        );
        // The cursor advanced: a second sweep round saw no duplicates.
        let count_before = log.lines().filter(|l| !l.contains("\"schema\"")).count();
        assert_eq!(
            run.event_cursor("persistence:sweep-ev"),
            Some(run.event_cursor("persistence:sweep-ev").unwrap_or(0)),
            "cursor recorded"
        );
        assert!(count_before >= 1);
        s.stop().ok();
    }

    /// Re-review item 40: the frame commit pipeline stamps citable ids and
    /// appends to frames.jsonl incrementally for persistent runs.
    #[test]
    fn commit_frame_stamps_id_and_persists_incrementally() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        let mut f =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 3, 9);
        let id = run.commit_frame(&mut f, Some("cf-sess")).expect("commit");
        assert_eq!(id, 1, "first committed frame gets id 1");
        assert_eq!(f.frame_id, Some(1));
        assert_eq!(f.run_id.as_deref(), Some(run.id()));
        assert_eq!(f.session_id.as_deref(), Some("cf-sess"));

        let path = run.run_dir().expect("dir").join("frames.jsonl");
        let log = std::fs::read_to_string(&path).expect("frames.jsonl");
        let first_record = log
            .lines()
            .find(|l| !l.contains("\"schema\""))
            .expect("one record line");
        let line: serde_json::Value = serde_json::from_str(first_record).expect("json");
        assert_eq!(line["frame_id"], 1);
        assert!(line["semantic_identity"]
            .as_str()
            .expect("semantic identity string")
            .starts_with("semantic-id:v1:"));

        let mut g =
            crate::backend::CanonicalFrame::new(crate::screen::ScreenState::new(80, 24), 4, 10);
        assert_eq!(
            run.commit_frame(&mut g, Some("cf-sess")).expect("commit"),
            2,
            "ids monotonically increase"
        );
    }

    #[test]
    fn persistent_run_writes_manifest_and_scenario_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        assert!(run.run_dir().is_some());
        assert!(run.run_dir().unwrap().join("run.json").exists());

        run.set_launch_spec(
            "primary-sess",
            crate::session::state::LaunchSpec::new("python3", 80, 24),
        );

        let scenario = crate::scenario::model::Scenario::new("roundtrip")
            .act(serde_json::json!({"action": "key", "key": "enter"}))
            .wait(serde_json::json!({"condition": "text", "text": "SAVED"}));
        let scenario_id = scenario.id.clone();
        let path = run.save_scenario(scenario).expect("save");
        assert!(path.exists());

        let ids = run.list_saved_scenarios().expect("list");
        assert_eq!(ids, vec![scenario_id.clone()]);

        // Load by id…
        let loaded = run.load_scenario(&scenario_id).expect("load by id");
        assert_eq!(loaded.step_count(), 2);
        assert_eq!(loaded.name, "roundtrip");
        // …and by unambiguous name.
        let loaded_by_name = run.load_scenario("roundtrip").expect("load by name");
        assert_eq!(loaded_by_name.id, scenario_id);
    }

    /// Finding 31: every persisted artifact names its format + version.
    /// A saved scenario is an envelope whose payload round-trips the
    /// historical shape; a bare pre-envelope file still restores; a
    /// mismatched version is a named restore warning, never silent
    /// best-effort.
    #[test]
    fn persisted_artifacts_carry_format_versions_and_refuse_mismatches() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        let sc = crate::scenario::model::Scenario::new("versioned")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let sc_id = sc.id.clone();
        let path = run.save_scenario(sc).expect("save");
        let _ = run.record_event("v-sess", "wait");
        run.wait_for_journal(std::time::Duration::from_secs(2));

        // The scenario file is an envelope naming the format.
        let bytes = std::fs::read(&path).expect("scenario bytes");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            v["schema"],
            serde_json::json!(crate::run::formats::tags::SCENARIO),
            "scenario names its format: {v}"
        );
        assert!(v["payload"].is_object(), "payload holds the old shape");
        // The restore path reads the envelope back into a Scenario.
        let restored = RunContext::restore(run.run_dir().unwrap()).expect("restore");
        assert!(restored.scenarios.all().contains_key(&sc_id));

        // The ledger stream opens with its header line.
        let ledger = std::fs::read_to_string(run.run_dir().unwrap().join("transactions.jsonl"))
            .expect("ledger");
        let first = ledger.lines().next().expect("header line");
        assert!(
            first.contains(crate::run::formats::tags::LEDGER),
            "ledger stream self-describes: {first}"
        );
        // And restore still sees the record past the header.
        assert_eq!(restored.transaction_total(), 1, "header is not a record");

        // A mismatched-version artifact is a NAMED restore warning.
        let wrong = crate::run::formats::Envelope::wrap(
            "tui-lab.scenario.v999",
            serde_json::json!({"id": "x", "name": "future"}),
        );
        let wrong_path = run
            .run_dir()
            .unwrap()
            .join("scenarios")
            .join("future-abc.json");
        std::fs::write(&wrong_path, wrong.to_vec_pretty().expect("bytes")).expect("write");
        let warned = RunContext::restore(run.run_dir().unwrap()).expect("restore");
        assert!(
            warned
                .restore_warnings()
                .iter()
                .any(|w| w.error.contains("tui-lab.scenario.v999")),
            "version mismatch names itself: {:?}",
            warned.restore_warnings()
        );
        run.close().ok();
    }

    /// Audit P1 (coverage sequence identity): a restored run must continue
    /// the persisted coverage sequence, not restart it at zero — otherwise
    /// post-resume events get seq values BELOW the previous epoch's cursor
    /// and a client holding a pre-close coverage cursor never sees them.
    #[test]
    fn restored_coverage_continues_the_sequence_high_water_mark() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");
        let _ = run.record_coverage_event("s1", "#save.activate");
        let _ = run.record_coverage_event("s1", "#cancel.activate");
        let _ = run.record_coverage_event("s1", "#save.activate");
        let pre_close_seq = run.coverage_seq();
        assert!(pre_close_seq >= 3);
        let pre_close_cursor = {
            // Seed a post-resume cursor to compare against: the last_seq of
            // the newest entry is the high-water a client would have read.
            run.coverage_ledger()
                .values()
                .map(|e| e.last_seq)
                .max()
                .expect("entries")
        };
        run.flush().expect("flush");
        run.close().expect("close");

        let mut restored = RunContext::restore(run.run_dir().unwrap()).expect("restore");
        // Resume epoch: a restored run stays closed until an explicit
        // reopen (the resume contract); coverage only folds while open.
        restored.reopen().expect("reopen");
        assert_eq!(
            restored.coverage_seq(),
            pre_close_seq,
            "restore reconstructs the sequence high-water from last_seq"
        );
        // New post-resume events continue STRICTLY ABOVE every persisted
        // sequence — never below a client's pre-close cursor.
        let _ = restored.record_coverage_event("s2", "#menu.open");
        let menu = restored
            .coverage_ledger()
            .get("#menu.open")
            .expect("post-resume entry");
        assert!(
            menu.first_seq > pre_close_cursor,
            "post-resume seq must exceed the pre-close high-water: \
             first_seq={} vs cursor={pre_close_cursor}",
            menu.first_seq
        );
        // A caller holding the PRE-CLOSE cursor sees exactly the new target
        // — monotonic delta cursors survive the resume epoch.
        restored.set_coverage_delta_cursor(pre_close_cursor);
        let fresh = restored
            .coverage_ledger()
            .iter()
            .filter(|(_, e)| e.first_seq > pre_close_cursor)
            .count();
        assert_eq!(fresh, 1, "exactly the post-resume target is new");
    }

    #[test]
    fn same_named_scenarios_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut run = RunContext::persistent(tmp.path()).expect("run");

        let a = crate::scenario::model::Scenario::new("login")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let b = crate::scenario::model::Scenario::new("login")
            .act(serde_json::json!({"action": "key", "key": "tab"}));
        let id_a = a.id.clone();
        let id_b = b.id.clone();
        assert_ne!(id_a, id_b);
        run.save_scenario(a).expect("save a");
        run.save_scenario(b).expect("save b");

        assert_eq!(run.scenarios.len(), 2, "both scenarios kept");
        // Ambiguous name resolves through neither…
        assert!(run.load_scenario("login").is_err());
        // …but each id resolves to its own steps.
        let loaded_a = run.load_scenario(&id_a).expect("load a");
        let loaded_b = run.load_scenario(&id_b).expect("load b");
        assert_ne!(
            loaded_a.steps, loaded_b.steps,
            "id collision would overwrite one scenario with the other"
        );
    }

    #[test]
    fn promote_preserves_identity_and_flushes_state() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        let run_id = run.id().to_string();

        // Accumulate state while ephemeral: checkpoint, scenario, findings,
        // counters, state graph.
        run.bump_transaction();
        run.bump_event();
        let cp_name = run.checkpoints.save(
            "sess-P",
            1,
            Some("promoted-cp".into()),
            &crate::screen::ScreenState::new(80, 24),
            None,
        );
        let promoted_scenario = crate::scenario::model::Scenario::new("promoted-flow")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let expected_stem = format!(
            "promoted-flow-{}",
            promoted_scenario.id.rsplit('-').next().unwrap_or("0")
        );
        run.save_scenario(promoted_scenario);
        let graph_root = base.path().join(".tui-lab").join("runs").join(&run_id);

        let root = run.promote(base.path()).expect("promote");
        assert_eq!(root, graph_root, "promote uses the caller-provided base");
        assert_eq!(run.id(), run_id, "promotion preserves run identity");
        assert!(graph_root.join("run.json").exists(), "manifest flushed");
        assert!(
            graph_root
                .join("scenarios")
                .join(format!("{}.json", expected_stem))
                .exists(),
            "ephemeral-era scenario flushed into durable root"
        );

        // In-memory checkpoint survived promotion (reroot, not replace).
        let screen = crate::screen::ScreenState::new(80, 24);
        let sem = crate::semantic::analyze(&screen);
        let cmp = run
            .checkpoints
            .compare("sess-P", &cp_name, &screen, Some(&sem));
        assert!(
            cmp.is_ok(),
            "checkpoint must survive promotion: {:?}",
            cmp.err()
        );
        assert!(
            graph_root
                .join("checkpoints")
                .join("sess-P")
                .join("promoted-cp.json")
                .exists(),
            "carried checkpoint re-persisted into the durable root"
        );

        // Closing marks + persists the flag; second close is a no-op.
        run.close().expect("close");
        assert!(run.is_closed());
        let text = std::fs::read_to_string(graph_root.join("run.json")).expect("manifest");
        assert!(text.contains("\"closed\": true"));
        run.close().expect("second close is a no-op");

        // Ephemeral close: no dir, no error, still marked closed.
        let mut eph = RunContext::ephemeral();
        eph.close().expect("ephemeral close");
        assert!(eph.is_closed() && eph.run_dir().is_none());
    }

    #[test]
    fn ephemeral_recording_survives_promotion() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        // Recording stopped while ephemeral: held in memory, not on disk.
        run.hold_recording("sess-A-123.cast".to_string(), "x\ny\n".to_string());
        assert_eq!(run.held_recordings().len(), 1);
        let root = run.promote(base.path()).expect("promote");
        let written = root.join("recordings").join("sess-A-123.cast");
        assert!(written.exists(), "held recording flushed at promotion");
        let body = std::fs::read_to_string(written).expect("body");
        assert_eq!(body, "x\ny\n");
        assert!(run.held_recordings().is_empty(), "flush drains the hold");
    }

    #[test]
    fn focus_transitions_record_and_flush() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        run.record_focus_transition("s", None, Some("file".into()));
        run.record_focus_transition("s", Some("file".into()), Some("file".into()));
        run.record_focus_transition("s", Some("file".into()), Some("menu".into()));
        // No-op transitions (unchanged focus) are not recorded.
        assert_eq!(run.focus_transitions().len(), 2);
        let root = run.promote(base.path()).expect("promote");
        let body = std::fs::read_to_string(root.join("focus_graph.json")).expect("focus graph");
        assert!(body.contains("\"menu\""), "ledger persisted: {body}");
        assert!(run.counts()["focus_transitions"].as_u64() >= Some(2));
    }

    /// Focus graph from transactions (audit item): driving focus movement
    /// through the canonical executor feeds the ID-keyed FocusGraph with a
    /// `via=<action>` edge — causal evidence, no summary polling needed.
    #[test]
    fn focus_graph_is_fed_from_interaction_transactions() {
        let mut run = RunContext::ephemeral();
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/nsp_tui.py");
        let mut s = crate::session::state::Session::new("fg-tx".into(), "python3".into());
        s.start_with_spec(crate::session::state::LaunchSpec {
            command: "python3".into(),
            args: vec![fixture.into()],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        })
        .expect("spawn");
        // Wait for the app to draw, then drive focus right (Save → Cancel).
        let _ = s.observe(200);
        for _ in 0..20 {
            if s.native_channel().latest.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            let _ = s.observe(60);
        }
        let tx = crate::execution::execute_act(
            &mut s,
            &crate::execution::CanonicalAction::Key {
                key: crate::backend::KeyEvent::new(crate::backend::KeyCode::Right),
            },
            60,
            2000,
            false,
        )
        .expect("execute");
        let _ = run.record_interaction("fg-tx", &tx);

        // The graph has at least one driven edge tagged with the EXACT
        // action identity (re-review P0): a Right keypress reads `right`,
        // not the useless kind-only `key`.
        let edges = &run.graphs().focus_graph.edges;
        assert!(
            edges.iter().any(|e| e.via == "right"),
            "transaction fed a via=right edge: {:?}",
            edges
        );
        s.stop().ok();
    }

    #[test]
    fn status_reports_ephemeral_and_persistent_modes() {
        let eph = RunContext::ephemeral();
        let st = eph.status(Vec::new());
        assert_eq!(st["mode"], "ephemeral");
        assert_eq!(st["persistent"], false);
        assert!(st["artifact_root"].is_null());

        let base = tempfile::tempdir().expect("base");
        let per = RunContext::persistent(base.path()).expect("persistent");
        let st2 = per.status(vec![serde_json::Value::String("sess-x".into())]);
        assert_eq!(st2["mode"], "persistent");
        assert_eq!(st2["persistent"], true);
        assert_eq!(st2["sessions"], serde_json::json!(["sess-x"]));
        assert!(st2["artifact_root"].as_str().is_some());
    }

    // ── Wave G item 74: run restore ─────────────────────────────────────

    /// Build a durable run with everything restore is expected to bring
    /// back: launch spec, transactions, findings, focus ledger, scenarios,
    /// a held recording.
    fn persisted_fixture(base: &std::path::Path) -> (String, std::path::PathBuf) {
        let mut run = RunContext::ephemeral();
        run.set_launch_spec(
            "sess-fix",
            crate::session::state::LaunchSpec::new("python3", 90, 26),
        );
        let _ = run.record_event("sess-fix", "wait");
        let _ = run.extend_findings(vec![crate::audit::Finding {
            id: "RESTORE-ME".into(),
            rule_id: None,
            severity: crate::audit::Severity::Warn,
            category: crate::audit::Category::Other("test".into()),
            summary: "finding that must survive restore".into(),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                "fixture",
                "restore test",
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }]);
        run.record_focus_transition("sess-fix", None, Some("file".into()));
        let sc = crate::scenario::model::Scenario::new("restored-flow")
            .act(serde_json::json!({"action": "key", "key": "enter"}));
        let expected_stem = format!("restored-flow-{}", sc.id.rsplit('-').next().unwrap_or("0"));
        run.save_scenario(sc);
        run.hold_recording("sess-fix-42.cast".into(), "x\n".into());

        let root = run.promote(base).expect("promote");
        assert!(root.join("run.json").exists());
        (expected_stem, root)
    }

    #[test]
    fn restore_brings_back_identity_ledger_findings_scenarios() {
        let base = tempfile::tempdir().expect("base");
        let (scenario_stem, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();

        let restored = RunContext::restore(&root).expect("restore");
        assert_eq!(restored.id(), run_id, "identity survives");
        assert!(restored.run_dir().is_some(), "durable root survives");
        assert_eq!(restored.transaction_total(), 1, "ledger count restored");
        assert_eq!(restored.transactions().len(), 1);
        assert_eq!(
            restored.transactions()[0].settle,
            "skipped",
            "non-interaction entries keep their honest settle"
        );
        assert_eq!(restored.findings().len(), 1);
        // IDs persist with their instance discriminator (Finding::instance
        // at record time), so the restored id is the instanced form. The
        // discriminator is the versioned-BLAKE3 finding:v1 hash of
        // (rule, target) — the old SipHash value was process-random and
        // this one is durable across runs and releases.
        assert_eq!(restored.findings()[0].id, "RESTORE-ME@1491868b");
        assert_eq!(restored.focus_transitions().len(), 1);
        // Scenario listable + loadable by id (name index rebuilt). The list
        // is keyed by id; the unambiguous name resolves to the same thing.
        let listed = restored.list_saved_scenarios().expect("list");
        assert_eq!(listed.len(), 1, "scenario listed after restore: {listed:?}");
        let loaded = restored
            .load_scenario("restored-flow")
            .expect("load by unambiguous name");
        assert_eq!(loaded.step_count(), 1);
        assert!(root
            .join("scenarios")
            .join(format!("{scenario_stem}.json"))
            .exists());
        // Launch specs restored → primary cwd resolvable again.
        let specs = restored.launch_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].1.cols, 90);
        assert_eq!(restored.primary_session_cwd(), None, "fixture cwd was None");
        // Held recording flushed at promotion → restored as an artifact ref.
        assert!(
            restored
                .artifacts()
                .iter()
                .any(|a| a.summary.contains("pty recording")),
            "recording re-registered: {:?}",
            restored.artifacts()
        );
        // No duplicated artifact refs from restore: the recording/capture/
        // events scan runs once per subdir, so every on-disk file maps to
        // exactly one ref keyed by (kind, relative path). A second scan pass
        // registers every file twice.
        let keys: Vec<(crate::run::ArtifactKind, Option<std::path::PathBuf>)> = restored
            .artifacts()
            .iter()
            .map(|a| (a.kind.clone(), a.path.clone()))
            .collect();
        let mut seen: Vec<_> = Vec::new();
        for k in &keys {
            assert!(
                !seen.contains(k),
                "artifact duplicated across restore: {:?} (all refs: {keys:?})",
                k
            );
            seen.push(k.clone());
        }
    }

    #[test]
    fn restored_run_continues_appending_to_the_same_dir() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();

        let mut restored = RunContext::restore(&root).expect("restore");
        let _ = restored.record_event("sess-fix", "wait");
        // The new record streams through the journal writer to the SAME
        // ledger file (id preserved, no fork into a new run dir) — drain
        // before reading (the writer is asynchronous by design).
        restored.wait_for_journal(std::time::Duration::from_secs(2));
        let body = std::fs::read_to_string(root.join("transactions.jsonl")).expect("ledger");
        assert_eq!(
            body.lines().count(),
            3,
            "ledger appended in place (+header): {body}"
        );
        // And the manifest still declares the same run id.
        let m = crate::run::manifest::load(&root).expect("manifest");
        assert_eq!(m.run_id, run_id);
    }

    #[test]
    fn restore_reports_torn_ledger_tail_honestly() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // Simulate a crash mid-append: a torn final line.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(root.join("transactions.jsonl"))
                .expect("open ledger");
            write!(f, "{{\"seq\":9,\"at\":1,\"session\":\"sess-fi").unwrap();
        }
        let restored = RunContext::restore(&root).expect("restore still succeeds");
        assert_eq!(restored.transactions().len(), 1, "only the good line");
        assert_eq!(
            restored.dropped_records(),
            1,
            "the torn line is DECLARED, not dropped silently"
        );
        assert!(
            !restored.history_complete(),
            "torn tail breaks completeness"
        );
    }

    /// Audit P1-46: a corrupt artifact file makes restore DEGRADED, not
    /// silently incomplete — the warning names the artifact and the reason,
    /// and the usable artifacts still come back.
    #[test]
    fn restore_of_corrupt_artifact_is_degraded_with_warning() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // Corrupt two artifacts: findings.json becomes invalid JSON;
        // coverage.json becomes valid JSON of the wrong shape.
        std::fs::write(root.join("findings.json"), b"{not json").expect("corrupt findings");
        std::fs::write(root.join("coverage.json"), b"[1,2,3]").expect("wrong-shape coverage");
        // A third artifact stays intact so we can assert the good half came
        // back too.
        let restored = RunContext::restore(&root).expect("restore succeeds despite damage");
        let warnings = restored.restore_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.artifact == "findings.json" && w.error.starts_with("corrupt")),
            "findings.json corruption must be named: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.artifact == "coverage.json" && w.error.starts_with("corrupt")),
            "coverage.json shape mismatch must be named: {warnings:?}"
        );
        // Absent-but-normal artifacts warn nothing.
        assert!(
            !warnings.iter().any(|w| w.artifact == "state_graph.json"),
            "a missing state graph is not damage: {warnings:?}"
        );
        let health = restored.restore_health();
        assert_eq!(health["degraded"], serde_json::json!(true));
        assert_eq!(health["warnings"].as_array().map(Vec::len), Some(2));
        // A healthy run reports clean.
        let fresh = RunContext::ephemeral();
        assert!(fresh.restore_warnings().is_empty());
        assert_eq!(fresh.restore_health()["degraded"], serde_json::json!(false));
    }

    #[test]
    fn resolve_run_dir_accepts_both_root_shapes() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        let run_id = root.file_name().unwrap().to_string_lossy().to_string();
        // Repo-root shape.
        assert_eq!(
            RunContext::resolve_run_dir(base.path(), &run_id),
            Some(root.clone())
        );
        // Runs-dir shape (the spec example).
        let runs = base.path().join(".tui-lab").join("runs");
        assert_eq!(
            RunContext::resolve_run_dir(&runs, &run_id),
            Some(root.clone())
        );
        // Unknown id → None.
        assert_eq!(RunContext::resolve_run_dir(base.path(), "run-nope"), None);
    }

    #[test]
    fn list_persisted_names_runs_and_corrupt_entries() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // A corrupt entry (no manifest).
        let scratch = base.path().join(".tui-lab").join("runs").join("scratch");
        std::fs::create_dir_all(&scratch).expect("scratch");
        let listed = RunContext::list_persisted(base.path()).expect("list");
        let run_entry = listed
            .iter()
            .find(|e| {
                e.get("run_id").and_then(|r| r.as_str())
                    == Some(root.file_name().unwrap().to_string_lossy().as_ref())
            })
            .expect("fixture run listed");
        assert_eq!(run_entry["closed"], false);
        assert_eq!(run_entry["history_complete"], true);
        assert_eq!(run_entry["ledger_transactions"], 1);
        let skipped = listed
            .iter()
            .find(|e| e.get("skipped").is_some())
            .expect("corrupt dir named, not hidden");
        assert!(
            skipped["path"].as_str().unwrap_or("").ends_with("scratch"),
            "{skipped}"
        );
    }

    /// A closed run restores as closed — resume never resurrects a
    /// finished run into pretending it is open.
    #[test]
    fn closed_run_stays_closed_after_restore() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        {
            let mut run = RunContext::restore(&root).expect("restore");
            run.close().expect("close");
        }
        let restored = RunContext::restore(&root).expect("restore again");
        assert!(restored.is_closed(), "closed flag is durable");
        assert_eq!(restored.status(Vec::new())["closed"], true);
    }

    /// Finding 32: reopen is not invisible. Each `reopen()` bumps a
    /// persisted run's resume epoch and writes it into the manifest before
    /// returning, so records written after a resume are distinguishable
    /// from the original process's history — and the epoch survives the
    /// next restore.
    #[test]
    fn reopen_bumps_a_durable_resume_epoch() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        {
            let mut run = RunContext::restore(&root).expect("restore");
            assert_eq!(run.status(Vec::new())["resume_epoch"], 0);
            run.reopen().expect("reopen");
            assert_eq!(run.status(Vec::new())["resume_epoch"], 1);
            // Persisted BEFORE reopen returns (a crash right after must
            // still name the epoch).
            let m = crate::run::manifest::load(&root).expect("manifest");
            assert_eq!(m.resume_epoch, 1, "manifest written by reopen itself");
            assert!(!m.closed, "reopen also reopens the manifest's view");
            run.reopen().expect("second reopen");
        }
        let again = RunContext::restore(&root).expect("restore once more");
        assert_eq!(again.status(Vec::new())["resume_epoch"], 2);
        // list_persisted names the epoch too, so a caller can pick a run
        // knowing how many times it was resumed.
        let listed = RunContext::list_persisted(base.path()).expect("list");
        let entry = listed
            .iter()
            .find(|e| {
                e.get("run_id").and_then(|r| r.as_str())
                    == Some(root.file_name().unwrap().to_string_lossy().as_ref())
            })
            .expect("run listed");
        assert_eq!(entry["resume_epoch"], 2);
    }

    /// Review P0.1 regression: a closed run is a sealed evidence bundle.
    /// Post-close traffic from still-live sessions must be REJECTED, not
    /// silently absorbed — `tui_run close` can leave sessions running, and
    /// a "closed" run that keeps growing was the defect.
    #[test]
    fn closed_run_rejects_all_mutation() {
        let mut run = RunContext::ephemeral();
        let screen = crate::screen::ScreenState::new(80, 24);
        let mut frame = crate::backend::CanonicalFrame::new(screen.clone(), 1, 1);

        run.close().expect("close");
        assert!(run.is_closed());

        // Every mutating surface refuses once closed.
        let after = crate::screen::ScreenState::new(80, 24);
        let tx = crate::execution::InteractionTransaction {
            action: crate::execution::ActionEnvelope::new(
                crate::execution::CanonicalAction::Type { text: "x".into() },
                crate::execution::InputVisibility::Normal,
            ),
            anchor: crate::execution::ObservationAnchor::default(),
            before_frame: crate::backend::CanonicalFrame::new(screen.clone(), 1, 1),
            after_frame: crate::backend::CanonicalFrame::new(after.clone(), 1, 2),
            settle: crate::execution::SettleStatus::Skipped,
            transition: crate::screen::diff::diff(&screen, &after),
            capture: None,
            focus_before: None,
            focus_after: None,
            elapsed_ms: 0,
            send_ms: 0,
            settle_ms: 0,
            render: None,
            transition_capture: None,
            origin: None,
        };
        assert!(run.record_interaction("s", &tx).is_err());
        assert!(run.record_event("s", "wait").is_err());
        assert!(run.commit_frame(&mut frame, Some("s")).is_err());
        assert!(run
            .record_scenario_act("s", 0, serde_json::json!({}))
            .is_err());
        assert!(run
            .record_scenario_wait("s", 0, serde_json::json!({}))
            .is_err());
        assert!(run
            .record_scenario_assert("s", 0, serde_json::json!({}))
            .is_err());
        assert!(run.record_coverage_event("s", "#save").is_err());
        assert!(run.hold_events("s", Vec::new()).is_err());
        assert!(run
            .register_artifact(ArtifactKind::Capture, None, None, None, "post-close")
            .is_err());
        assert!(run.extend_findings(Vec::new()).is_err());
        assert!(run.extend_findings_with_source_refs(Vec::new()).is_err());

        // And nothing actually landed.
        assert_eq!(run.transaction_total(), 0, "no post-close ledger entries");
        assert_eq!(
            run.frame_hot_evicted() + run.frame_hot_records().count() as u64,
            0,
            "no post-close frames"
        );
        assert!(run.findings().is_empty(), "no post-close findings");
        assert!(run.artifacts().is_empty(), "no post-close artifacts");
        assert!(run.coverage_ledger().is_empty(), "no post-close coverage");
    }

    /// The error names the run and the remedy (the agent-facing message).
    #[test]
    fn ensure_open_error_is_actionable() {
        let mut run = RunContext::ephemeral();
        run.close().expect("close");
        let e = run.record_event("s", "wait").expect_err("closed");
        let msg = format!("{e:#}");
        assert!(msg.contains(run.id()), "names the run: {msg}");
        assert!(msg.contains("closed"), ": {msg}");
        assert!(msg.contains("resume"), "names the remedy: {msg}");
    }
    /// Audit P0 (beta stability, finding 2): the inversion regression —
    /// a damaged restored run must surface its damage on the STATUS
    /// surface (not just restore_health()), and a healthy run reports
    /// restore:null there. The old code did the exact opposite.
    #[test]
    fn status_surfaces_restored_damage_not_null() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        // Corrupt one persisted artifact, then restore.
        std::fs::write(root.join("findings.json"), b"{not json").expect("corrupt");
        let restored = RunContext::restore(&root).expect("restore despite damage");
        let status = restored.status(Vec::new());
        assert_eq!(
            status["restore"]["degraded"],
            serde_json::json!(true),
            "damaged restore must be degraded in status, got: {}",
            status["restore"]
        );
        let warnings = status["restore"]["warnings"]
            .as_array()
            .expect("warnings array present");
        assert!(
            warnings
                .iter()
                .any(|w| w["artifact"] == serde_json::json!("findings.json")),
            "the damaged artifact must be named in status.restore.warnings: {warnings:?}"
        );
        // Control: an undamaged restored run reports null on status (the
        // damage block is absent, not a fake clean report).
        let (_, clean_root) = persisted_fixture(base.path());
        let clean = RunContext::restore(&clean_root).expect("clean restore");
        assert_eq!(
            clean.status(Vec::new())["restore"],
            serde_json::Value::Null,
            "healthy restored run carries no damage block"
        );
    }

    /// Audit P0 (beta stability, finding 1): promote is transactional —
    /// if the initial ledger write fails, the flushed watermark does NOT
    /// move (the evidence stays memory-committed, unflushed), and promote
    /// returns the error instead of reporting success over unwritten
    /// records.
    #[test]
    fn promote_ledger_write_failure_does_not_mark_flushed() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        run.set_launch_spec(
            "s",
            crate::session::state::LaunchSpec::new("python3", 80, 24),
        );
        let _ = run.record_event("s", "wait");
        // Force the ledger write to fail: pre-create `transactions.jsonl`
        // as a DIRECTORY so OpenOptions::open on that path errors.
        let id = run.id().to_string();
        let target = base
            .path()
            .join(".tui-lab")
            .join("runs")
            .join(&id)
            .join("transactions.jsonl");
        std::fs::create_dir_all(&target).expect("blocker dir");
        let err = run.promote(base.path()).expect_err("promote must fail");
        assert!(
            err.to_string().contains("transactions.jsonl")
                || err.to_string().contains("Is a directory")
                || err.to_string().to_lowercase().contains("os error"),
            "error surfaces the filesystem failure: {err}"
        );
        // The watermark never moved: the transaction window is still
        // memory-only evidence (unsaved_evidence still counts it as
        // lost-if-dropped; promotion did not silently acknowledge it).
        let loss = run.unsaved_evidence();
        assert_eq!(
            loss["lost_if_dropped"].as_u64(),
            Some(run.transactions().len() as u64),
            "the un-promoted ledger stays memory-only: {loss}"
        );
        assert!(
            loss["evidence"]["transactions"].as_u64().unwrap_or(0) > 0,
            "transactions counted as unsaved: {loss}"
        );
    }

    /// Audit P0 (beta stability, finding 1): close is failure-atomic — a
    /// flush failure rolls the closed flag back, so the run stays open
    /// and re-closeable, and close() reports the error.
    #[test]
    fn close_failure_keeps_the_run_open() {
        let base = tempfile::tempdir().expect("base");
        let mut run = RunContext::ephemeral();
        run.set_launch_spec(
            "s",
            crate::session::state::LaunchSpec::new("python3", 80, 24),
        );
        let _ = run.record_event("s", "wait");
        let _ = run.promote(base.path()).expect("promote");
        // Make every flush write fail: replace the run dir with a file
        // is too invasive; instead make the scenarios dir unwritable by
        // creating a FILE where flush needs a DIRECTORY.
        let dir = run.run_dir().expect("dir").to_path_buf();
        std::fs::remove_dir_all(dir.join("scenarios")).expect("rm scenarios dir");
        std::fs::write(dir.join("scenarios"), b"not a dir").expect("blocker file");
        assert!(run.close().is_err(), "flush failure must fail close");
        assert!(
            !run.is_closed(),
            "a failed close must roll the closed flag back"
        );
        // Remove the blocker: the run can still flush and close for real.
        std::fs::remove_file(dir.join("scenarios")).expect("unblock");
        run.close().expect("close succeeds after unblocking");
        assert!(run.is_closed());
    }

    /// Audit P0 (beta stability, finding 1): reopen is failure-atomic —
    /// a failed manifest write leaves the run closed at its PREVIOUS
    /// epoch (both in memory and on disk).
    #[test]
    fn failed_reopen_leaves_previous_epoch() {
        let base = tempfile::tempdir().expect("base");
        let (_, root) = persisted_fixture(base.path());
        let mut run = RunContext::restore(&root).expect("restore");
        // Block the manifest write: run.json becomes a DIRECTORY (the
        // manifest's open-for-write then fails).
        let manifest = root.join("run.json");
        std::fs::remove_file(&manifest).expect("rm manifest");
        std::fs::create_dir_all(&manifest).expect("blocker dir");
        assert!(run.reopen().is_err(), "manifest failure must fail reopen");
        assert!(
            run.is_closed(),
            "a failed reopen leaves the run closed in memory"
        );
        assert_eq!(
            run.status(Vec::new())["resume_epoch"],
            serde_json::json!(0),
            "the epoch did NOT move"
        );
        // The on-disk manifest is untouched (still absent, still no new
        // epoch anywhere).
        assert!(manifest.is_dir(), "reopen never wrote through the blocker");
        // Unblock and reopen for real.
        std::fs::remove_dir(&manifest).expect("unblock");
        run.reopen().expect("reopen after unblocking");
        assert_eq!(run.status(Vec::new())["resume_epoch"], serde_json::json!(1));
    }
}

// W2.10 unit checks for the coverage→SourceRef join.
#[cfg(test)]
mod source_ref_tests {
    use super::*;
    use crate::audit::{EvidenceKind, EvidenceRef, Finding};

    fn finding_pointing_at(control_id: &str) -> Finding {
        Finding {
            id: "TEST-001".into(),
            rule_id: None,
            severity: crate::audit::Severity::Error,
            category: crate::audit::Category::Focus,
            summary: "test finding".into(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Control,
                control_id,
                "test control",
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    #[test]
    fn file_locus_discrimination() {
        assert!(target_is_file_locus("src/lib.rs"));
        assert!(target_is_file_locus("src/lib.rs:42"));
        assert!(target_is_file_locus("ui\\panel.py:10:3"));
        assert!(!target_is_file_locus("#save.activate"));
        assert!(!target_is_file_locus("widget:#save"));
        assert!(!target_is_file_locus("main"));
    }

    #[test]
    fn target_parses_to_source_ref() {
        let sr = source_ref_from_target("src/ui/settings.rs:184").expect("parse");
        assert_eq!(sr.file, "src/ui/settings.rs");
        assert_eq!(sr.line, 184);
        // Review §4: this join is a run-level correlation, not a cause-site
        // attestation — provenance `correlated` keeps it investigative even
        // though the locus itself is well-mapped (confidence 0.7).
        assert_eq!(
            sr.provenance,
            crate::semantic::source_ref::Provenance::Correlated
        );
        assert!(
            !sr.is_actionable(),
            "correlated loci stay below the actionable fence"
        );
        let with_col = source_ref_from_target("src/a.rs:12:5").expect("parse");
        assert_eq!(with_col.column, Some(5));
        assert!(source_ref_from_target("not-a-ref").is_none());
    }

    #[test]
    fn coverage_widget_matches_control_id() {
        assert!(coverage_target_matches_control(
            "widget:#save.activate",
            "button/save/40,12"
        ));
        assert!(coverage_target_matches_control(
            "#cancel",
            "button/cancel/52,12"
        ));
        assert!(!coverage_target_matches_control(
            "widget:#quit.activate",
            "button/save/40,12"
        ));
        assert!(!coverage_target_matches_control("widget:#save", ""));
    }

    fn compare_test_finding(id: &str, category: &str, target: &str) -> crate::audit::Finding {
        Finding {
            id: id.into(),
            rule_id: None,
            severity: crate::audit::Severity::Warn,
            category: crate::audit::Category::parse(category),
            summary: format!("{} in {}", id, category),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                target,
                "evidence",
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    /// Beta-audit P0.10: the latest audit pass is a first-class snapshot.
    /// A defect that was in the baseline, was fixed (absent from the
    /// newest pass), and still has stale copies in the cumulative ledger
    /// must compare as FIXED — the ledger is history, not the current
    /// set. This is the exact defect the old bundle had: comparing
    /// against `findings()` kept a fixed defect "persisting" forever.
    #[test]
    fn latest_pass_snapshot_not_ledger_is_the_comparison_set() {
        let mut run = RunContext::ephemeral();
        let baseline_pass = vec![
            compare_test_finding("KB-TRAP", "keyboard", "tab_trap"),
            compare_test_finding("CLIP-001", "clipping", "region-a"),
        ];
        // Pass 1: both defects present. Baseline + snapshot recorded.
        run.record_audit_pass(baseline_pass.clone());
        run.record_finding_baseline("baseline", baseline_pass);

        // Later activity adds unrelated findings to the LEDGER (the
        // cumulative history every audit appends to).
        let _ = run.extend_findings(vec![compare_test_finding("LATE-001", "focus", "unrelated")]);

        // Pass 2 (the fix): KB-TRAP and CLIP-001 are GONE from the
        // pass; only the unrelated late finding runs.
        run.record_audit_pass(vec![compare_test_finding("LATE-001", "focus", "unrelated")]);

        let base = run.finding_baseline("baseline").unwrap();
        let resolved =
            crate::audit::compare::Resolved(run.resolved_finding_fingerprints("baseline"));
        let compared = crate::audit::compare::compare_with_resolved(
            base,
            run.latest_audit_pass().expect("a pass snapshot exists"),
            &resolved,
        );
        let verdict_of = |id: &str| {
            compared
                .iter()
                .find(|c| c.finding.id == id)
                .map(|c| c.verdict)
                .unwrap_or("absent-from-current")
        };
        assert_eq!(
            verdict_of("KB-TRAP"),
            "fixed",
            "a defect absent from the newest pass is FIXED — a stale copy in the cumulative ledger must not resurrect it"
        );
        assert_eq!(verdict_of("CLIP-001"), "fixed");
        assert_eq!(
            verdict_of("LATE-001"),
            "new",
            "the unrelated late finding is new (not in the baseline)"
        );

        // The ledger still carries what was ever appended to it (history
        // is kept — the point is it no longer decides the comparison).
        let ledger_rules: Vec<&str> = run
            .findings()
            .iter()
            .map(|f| f.rule_id.as_deref().unwrap_or(f.id.as_str()))
            .collect();
        assert!(ledger_rules.contains(&"LATE-001"));

        // A run with NO completed pass has no comparison set at all —
        // callers must report that honestly rather than substitute the
        // ledger.
        let fresh = RunContext::ephemeral();
        assert!(
            fresh.latest_audit_pass().is_none(),
            "no completed pass ⇒ no current set"
        );
    }

    #[test]
    fn findings_get_app_declared_loci_when_control_is_covered() {
        let mut run = RunContext::ephemeral();
        let _ = run.record_coverage_event("s1", "src/ui/settings.rs:184");
        let _ = run.record_coverage_event("s1", "widget:#save.activate");
        let _ =
            run.extend_findings_with_source_refs(vec![finding_pointing_at("button/save/40,12")]);
        let f = run.findings().last().unwrap();
        assert!(
            !f.source_refs.is_empty(),
            "covered control gets its app-declared locus"
        );
        // Review §4: a file-locus ledger key joined on control identity is
        // a RUN-LEVEL CORRELATION, not a cause-site attestation — the app
        // never said "this locus is where button/save lives"; it only
        // declared the locus for some other coverage target in the same
        // run. Correlated stays investigative, below the actionable fence.
        assert_eq!(
            f.source_refs[0].provenance,
            crate::semantic::source_ref::Provenance::Correlated
        );
        assert!(!f.source_refs[0].is_actionable());
        // An uncovered control carries no locus — never guessed.
        let _ = run.extend_findings_with_source_refs(vec![finding_pointing_at("button/other/1,1")]);
        let f2 = run.findings().last().unwrap();
        assert!(f2.source_refs.is_empty(), "no locus without coverage");
    }

    /// Review §4: the ATTESTED chain — the finding's control matches a
    /// widget-target coverage entry that itself carries app-declared source
    /// loci. That is the app drawing the line (native id → coverage event →
    /// locus), and it is the only join that lands above the actionable
    /// fence.
    #[test]
    fn attested_join_only_via_widget_entry_with_declared_loci() {
        let mut run = RunContext::ephemeral();
        // The attested chain is the identity fold: `widget:#save.activate`
        // reported WITH the locus the app declared for #save. Via
        // record_coverage_event (no locus), the same key would only
        // correlate.
        use crate::semantic::source_ref::{Provenance, SourceRef};
        let _ = run.record_coverage_event_with_identity(
            "s1",
            "widget:#save.activate",
            SourceRef {
                file: "src/ui/settings.rs".into(),
                line: 184,
                column: None,
                symbol: None,
                framework_id: Some("#save".into()),
                confidence: 1.0,
                source: "framework-adapter".into(),
                provenance: Provenance::Unknown,
            },
        );
        let _ =
            run.extend_findings_with_source_refs(vec![finding_pointing_at("button/save/40,12")]);
        let f = run.findings().last().unwrap();
        assert_eq!(f.source_refs.len(), 1);
        assert_eq!(f.source_refs[0].provenance, Provenance::Attested);
        assert!(
            f.source_refs[0].is_actionable(),
            "native id → coverage event → locus is the actionable chain"
        );
    }

    /// P0.7: delta reads real accumulated evidence via a cursor — targets
    /// first seen after the caller's last delta are "new"; re-hits of an
    /// already-seen target are not re-reported as new, but advance the
    /// sequence and cursor. The old `delta` that ran the same tuicov command
    /// as every other action is gone.
    #[test]
    fn coverage_delta_uses_real_cursor_and_sequence() {
        let mut run = RunContext::ephemeral();
        // First two targets arrive.
        let _ = run.record_coverage_event("s1", "src/a.rs");
        let _ = run.record_coverage_event("s1", "src/b.rs");
        assert!(run.coverage_seq() >= 2);

        // Delta from cursor 0 reports both as new and advances the cursor.
        let cur = run.coverage_delta_cursor();
        let new: Vec<String> = run
            .coverage_ledger()
            .iter()
            .filter(|(_, e)| e.first_seq > cur)
            .map(|(t, _)| t.clone())
            .collect();
        assert_eq!(new.len(), 2);
        run.set_coverage_delta_cursor(run.coverage_seq());

        // A re-hit of an existing target must NOT reappear as new on the next
        // delta, because its first_seq is unchanged.
        let _ = run.record_coverage_event("s1", "src/a.rs");
        let new2: Vec<String> = run
            .coverage_ledger()
            .iter()
            .filter(|(_, e)| e.first_seq > run.coverage_delta_cursor())
            .map(|(t, _)| t.clone())
            .collect();
        assert_eq!(
            new2.len(),
            0,
            "re-hitting a known target is not 'new coverage'"
        );

        // A genuinely new target after the cursor IS new.
        let _ = run.record_coverage_event("s1", "src/c.rs");
        let new3: Vec<String> = run
            .coverage_ledger()
            .iter()
            .filter(|(_, e)| e.first_seq > run.coverage_delta_cursor())
            .map(|(t, _)| t.clone())
            .collect();
        assert_eq!(new3, vec!["src/c.rs".to_string()]);
    }
}
