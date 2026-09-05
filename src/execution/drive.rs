//! The central machine-driving pipeline (audit P0-1).
//!
//! `tui_act` was the only driver that ran the full evidence pipeline —
//! guarded canonical execution, frame commits, ledger transaction,
//! scenario capture, event/coverage folding. Every other driver (intent,
//! probe stimulus, scenario replay, exploration, audit drivers)
//! re-implemented a SUBSET, so each one silently dropped some invariant
//! the act path enforced. This module is the one pipeline:
//!
//! ```text
//! canonical execute (guard validated atomically with the send)
//!   → commit before/after frames
//!   → transaction ledger record
//!   → scenario capture (sensitivity-preserving)
//!   → fold session events (persistence + native coverage)
//! ```
//!
//! Authorization (the human-control lease) and envelope error mapping
//! stay one layer up (`mcp::tools::handlers::drive`), because they speak
//! the MCP error vocabulary; everything that can change the target TUI
//! funnels through here so the invariants stop being something handlers
//! remember.

use crate::execution::CanonicalAction;
use crate::run::RunContext;
use rmcp::serde_json::json;

/// Scenario capture for one driven act: the wire-shape request, so a
/// recording replays the exact step the caller sent. Sensitive acts are
/// recorded as `${PARAM}` references (payload stripped) — the same policy
/// `tui_act` has always applied, now applied by the boundary instead of
/// by each driver.
#[derive(Debug, Clone)]
pub struct ScenarioCapture {
    /// The act step params, in the `TuiActRequest` wire grammar.
    pub params: serde_json::Value,
    /// Whether the caller marked the payload sensitive.
    pub sensitive: bool,
}

/// Everything one driven act needs, beyond the action itself.
pub struct DriveSpec<'a> {
    pub action: &'a CanonicalAction,
    pub quiet_ms: u64,
    pub budget_ms: u64,
    pub no_wait: bool,
    pub visibility: crate::execution::InputVisibility,
    pub completion: crate::capture::CompletionPolicy,
    pub guard: Option<&'a crate::execution::MutationGuard>,
    /// `None` = do not touch scenario recordings (e.g. internal plan
    /// steps that are not themselves caller actions).
    pub scenario: Option<ScenarioCapture>,
    /// Finding 2: which subsystem is driving. Stamped on the transaction
    /// and the ledger row, so "WHO sent this input" is recorded evidence,
    /// not an inference from context.
    pub origin: crate::execution::DriveOrigin,
}

/// Finding 9: how healthy the run-level evidence for one driven act is.
/// Frame commits and the ledger record CAN fail (run closed mid-drive,
/// disk full, append error) — before this model the failures were
/// `.unwrap_or_default()`-ed into frame id 0 and the outcome still cited
/// `"frame:0"` as if it were evidence. Health names exactly which
/// evidence legs committed and which did not, so a consumer citing the
/// run knows what stands behind it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvidenceHealth {
    /// Per-frame commit result, in [before, after] order: `Ok(frame_id)`
    /// or `Err(reason)`.
    pub frame_commits: [Result<u64, String>; 2],
    /// Whether the interaction transaction was recorded in the run
    /// ledger (the reconstructable record; sensitive payloads redacted).
    pub ledger_recorded: bool,
}

impl EvidenceHealth {
    /// True only when every evidence leg committed. A `false` here does
    /// not invalidate the act — the TUI still received the input (the
    /// execution itself succeeded) — it means the run's citable record
    /// is incomplete and the response says so.
    pub fn healthy(&self) -> bool {
        self.frame_commits.iter().all(|r| r.is_ok()) && self.ledger_recorded
    }

    /// Human-readable list of what failed, for warnings surfaces.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        let names = ["before", "after"];
        for (i, r) in self.frame_commits.iter().enumerate() {
            if let Err(reason) = r {
                out.push(format!("{} frame not committed: {reason}", names[i]));
            }
        }
        if !self.ledger_recorded {
            out.push("interaction not recorded in the run ledger".to_string());
        }
        out
    }

    /// The wire shape: committed ids stay `"frame:N"`; failed legs are
    /// `null` with the reason beside them, never a fabricated default id.
    pub fn frames_json(&self) -> serde_json::Value {
        let leg = |r: &Result<u64, String>| match r {
            Ok(id) => json!({ "ref": format!("frame:{id}") }),
            Err(reason) => json!({ "ref": serde_json::Value::Null, "error": reason }),
        };
        json!({
            "before": leg(&self.frame_commits[0]),
            "after": leg(&self.frame_commits[1]),
        })
    }
}

/// The evidence one driven act produced.
pub struct DriveOutcome {
    /// The full interaction transaction (frames, settle, transition,
    /// render evidence).
    pub tx: crate::execution::InteractionTransaction,
    /// Frame references: `{"before": "frame:N", "after": "frame:M"}`.
    pub frames: serde_json::Value,
    /// Finding 9: how much of that evidence actually committed to the
    /// run. The act succeeding and the evidence committing are separate
    /// facts; both are reported.
    pub health: EvidenceHealth,
}

/// THE driving pipeline. Runs inside the session's actor (call it from a
/// `with_sess` closure only): guarded execute → frames → ledger →
/// scenario → event/coverage fold.
pub fn drive(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<RunContext>>,
    spec: DriveSpec<'_>,
) -> Result<DriveOutcome, anyhow::Error> {
    let tx = crate::execution::execute_act_with_guard_and_origin(
        sess,
        spec.origin,
        spec.action,
        spec.quiet_ms,
        spec.budget_ms,
        spec.no_wait,
        spec.visibility,
        spec.completion,
        spec.guard,
    )?;
    // Commit evidence under one short lock on the actor thread. Finding 9:
    // commit results are RECORDED, not swallowed — a failed frame commit
    // used to `.unwrap_or_default()` into frame id 0 and the outcome still
    // cited "frame:0" as if it were evidence. The act itself already
    // happened either way; health says which legs of the run's record
    // stand behind it.
    let (frames, health) = {
        let (sid, gen) = (sess.id.clone(), sess.generation);
        let mut run = run.lock().unwrap();
        // Frame commit pipeline (re-review item 40): both frames through
        // the ONE commit path — id + provenance + incremental append.
        let mut commit = |f: &crate::backend::CanonicalFrame| {
            let mut f = f.clone();
            f.session_id = Some(sid.clone());
            f.generation = Some(gen);
            run.commit_frame(&mut f, Some(&sid))
                .map_err(|e| e.to_string())
        };
        let frame_commits = [commit(&tx.before_frame), commit(&tx.after_frame)];
        // Run ledger (Wave-2 item 15): the reconstructable transaction
        // record. Sensitive payloads are projected to Redacted(kind,
        // byte_len) by the ledger itself.
        let ledger_recorded = run.record_interaction(&sid, &tx).is_ok();
        // Scenario capture (re-review P0.3): sensitive text payloads are
        // recorded as ${PARAM} references; sensitive non-text payloads as
        // an opaque redacted step; everything else verbatim.
        if let Some(cap) = &spec.scenario {
            let recorded = if cap.sensitive {
                match spec.action {
                    CanonicalAction::Type { .. } => Some("text"),
                    CanonicalAction::Paste { .. } => Some("paste"),
                    _ => None,
                }
            } else {
                None
            };
            match recorded {
                Some(field) => {
                    let _ = run.record_scenario_act_sensitive(
                        &sid,
                        gen,
                        cap.params.clone(),
                        field,
                        crate::scenario::model::SensitiveKind::Secret,
                        spec.action.payload_len(),
                    );
                }
                None if cap.sensitive => {
                    let _ = run.record_scenario_act(
                        &sid,
                        gen,
                        json!({
                            "action": spec.action.name(),
                            "sensitive": true,
                            "redacted": true,
                            "payload_bytes": spec.action.payload_len(),
                        }),
                    );
                }
                None => {
                    let _ = run.record_scenario_act(&sid, gen, cap.params.clone());
                }
            }
        }
        let health = EvidenceHealth {
            frame_commits,
            ledger_recorded,
        };
        let frames = health.frames_json();
        (frames, health)
    };
    // Universal evidence fold (audit §28): the session's event queue lands
    // in the run (incremental persistence + native coverage) after EVERY
    // driven act, not only on observe sweeps.
    fold_session_events(sess, run);
    Ok(DriveOutcome { tx, frames, health })
}

/// Fold the session's event queue into the run: incremental event
/// persistence via the run's own consumer cursor, then native coverage
/// ingestion. Extracted from the observe `sweep` so every driving path
/// shares one fold; idempotent per event (seqs are unique).
pub fn fold_session_events(
    sess: &mut crate::session::Session,
    run: &std::sync::Arc<std::sync::Mutex<RunContext>>,
) {
    let mut run = run.lock().unwrap();
    {
        let cursor_key = format!("persistence:{}", sess.id);
        let from = run.event_cursor(&cursor_key).unwrap_or(0);
        let batch = sess.events_since(from);
        if !batch.events.is_empty() {
            let _ = run.hold_events(&sess.id, batch.events);
            run.set_event_cursor(&cursor_key, batch.cursor);
        }
    }
    run.bump_event();
    // Coverage ingestion rides every fold (re-review P1 item 17): native
    // coverage events were absorbed into the session queue by observe
    // itself, so the run ledger folds them here.
    let batch = sess.events_since(0);
    let cursor_key = format!("coverage:{}", sess.id);
    let from = run.event_cursor(&cursor_key).unwrap_or(0);
    let seq = batch.cursor;
    if batch.cursor > from {
        let evs = sess.events_since(from);
        // Wave 5 item 41: widget-targeted coverage events join the
        // declaring node's app-attested source locus (same NSP channel),
        // so coverage→source is exact. File targets stay plain.
        for ev in &evs.events {
            if let crate::events::TerminalEventKind::NativeEvent { event, target } = &ev.kind {
                if event != "coverage" {
                    continue;
                }
                let locus = sess.native_source_for_widget(target).filter(|_| {
                    !(target.starts_with("src/") || target.contains(".rs:") || target.contains('/'))
                });
                match locus {
                    Some(sr) => {
                        let _ = run.record_coverage_event_with_identity(&sess.id, target, sr);
                    }
                    None => {
                        let _ = run.record_coverage_event(&sess.id, target);
                    }
                }
            }
        }
        run.set_event_cursor(&cursor_key, seq);
    }
}

/// Serialize a [`CanonicalAction`] into the `TuiActRequest` wire grammar
/// scenario act steps replay. (The canonical enum's own serde uses
/// `{"kind":...}`; scenario act steps parse `{"action":...}` — this is
/// the one bridge.)
pub fn act_request_json(action: &CanonicalAction) -> serde_json::Value {
    use crate::backend::{MouseButton as MB, ScrollDirection as SD};
    fn button_name(b: MB) -> &'static str {
        match b {
            MB::Left => "left",
            MB::Middle => "middle",
            MB::Right => "right",
        }
    }
    fn direction_name(d: SD) -> &'static str {
        match d {
            SD::Up => "up",
            SD::Down => "down",
        }
    }
    match action {
        CanonicalAction::Key { key } => json!({ "action": "key", "key": key.display() }),
        CanonicalAction::Keys { keys } => json!({
            "action": "keys",
            "keys": keys.iter().map(|k| k.display()).collect::<Vec<_>>(),
        }),
        CanonicalAction::Type { text } => json!({ "action": "type", "text": text }),
        CanonicalAction::Paste { text } => json!({ "action": "paste", "paste": text }),
        CanonicalAction::Raw { bytes } => json!({ "action": "raw", "raw": bytes }),
        CanonicalAction::MouseClick { button, x, y } => {
            json!({ "action": "mouse_click", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MousePress { button, x, y } => {
            json!({ "action": "mouse_press", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseRelease { button, x, y } => {
            json!({ "action": "mouse_release", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseMove { x, y } => json!({ "action": "mouse_move", "x": x, "y": y }),
        CanonicalAction::MouseDrag { button, x, y } => {
            json!({ "action": "mouse_drag", "button": button_name(*button), "x": x, "y": y })
        }
        CanonicalAction::MouseScroll { direction, x, y } => json!({
            "action": "mouse_scroll", "direction": direction_name(*direction), "x": x, "y": y,
        }),
        CanonicalAction::Resize { cols, rows } => {
            json!({ "action": "resize", "cols": cols, "rows": rows })
        }
        CanonicalAction::Signal { signal } => json!({ "action": "signal", "signal": signal }),
    }
}
