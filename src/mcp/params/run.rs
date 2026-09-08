//! tui_run parameters.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use super::common::selector_enum;
use super::*;
use serde::{Deserialize, Serialize};

selector_enum!(
    /// `tui_run` action. `list` (Wave G item 75) enumerates persisted runs;
    /// `resume` (Wave G item 74) restores one as the server's live run;
    /// `diagnose` (review §2, formerly `repair`) returns diagnostic
    /// evidence contexts per finding.
    RunAction;
    [
        Status => "status", Persist => "persist", Close => "close",
        Context => "context", List => "list", Resume => "resume",
        // Review §2 / beta-audit P1.6: diagnosis, not repair — the
        // contexts carry evidence, provenance-tiered loci, verification
        // plans, and next observations; they never prescribe edits. The
        // pre-beta `repair` alias is GONE: carrying a name whose meaning
        // reversed ("repair" that does not repair) would confuse agents
        // forever.
        Diagnose => "diagnose",
        // Review P0.1/2: begin a fresh ephemeral run — the clean "next run"
        // operation. Starts a new evidence bundle; any live sessions from
        // the prior run become foreign owners (refused until stopped).
        New => "new",
        // Wave 5 item 46: one finding's diagnostic context joined with the
        // finding-baseline diff (before/after) — the "did the change hold
        // without regressing anything?" bundle.
        Bundle => "bundle",
    ]
);

/// Run lifecycle (goal spec): status / persist / close / context. Ephemeral
/// by default; `persist` promotes the SAME run to durable storage.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TuiRunParams {
    pub action: Known<RunAction>,
    /// persist: explicit artifact root. When omitted, resolved from the
    /// primary session's `LaunchSpec.cwd` — never this process's cwd.
    /// list: base to scan (same resolution order). resume: base under which
    /// the run lives.
    #[serde(default)]
    pub root: Option<String>,
    /// close: kill sessions too? (default false — close never touches
    /// sessions unless explicitly told to).
    #[serde(default)]
    pub kill_sessions: Option<bool>,
    /// resume: the run id to restore (from `tui_run action=list`). Either
    /// this or `run_dir` must be given.
    #[serde(default)]
    pub run_id: Option<String>,
    /// resume: the run directory directly (as listed by `action=list`'s
    /// `dir` field) — for callers that already hold the path.
    #[serde(default)]
    pub run_dir: Option<String>,
    /// bundle: the finding id to bundle (Wave 5 item 46).
    #[serde(default)]
    pub finding_id: Option<String>,
    /// bundle: the labeled baseline to diff against (the audit pass the
    /// change under investigation is verified against; from tui_audit
    /// label=...).
    #[serde(default)]
    pub compare_to: Option<String>,
    /// resume: stop any live sessions that belong to a DIFFERENT run before
    /// restoring? (default false — resume refuses when foreign sessions are
    /// still live, to preserve run provenance; pass true to detach them).
    #[serde(default)]
    pub detach_existing_sessions: Option<bool>,
    /// new: deliberately abandon the current EPHEMERAL run's in-memory
    /// evidence? (finding 33 — default false: `new` REFUSES when an
    /// ephemeral run holds evidence, naming what would be lost; pass
    /// `discard=true` (or `persist` first) to proceed. A persistent run is
    /// never affected — its evidence is already on disk and `new` flushes
    /// it before the swap.)
    #[serde(default)]
    pub discard: Option<bool>,
}
