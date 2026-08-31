//! UX audit engine: deterministic rules + evidence (spec section 14-21).
//! The LLM (Hermes) may interpret results afterward; the engine only emits
//! findings with evidence.

use crate::screen::ScreenState;
use crate::semantic::{Affordance, Control, ControlKind, Invocation, SemanticScreen, Visibility};
use serde_json::json;
use std::path::PathBuf;

pub mod driver;
pub mod orchestrator;
pub use driver::*;

/// What kind of artifact an [`EvidenceRef`] points at.
///
/// Replaces the old `evidence: serde_json::Value` shape with a typed
/// discriminator (re-review Part XV). Findings can carry multiple refs;
/// each one names a single, citable piece of evidence so the LLM and the
/// edit/re-test cycle can resolve them deterministically.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A `ScreenState` snapshot, optionally identified by structure hash.
    ScreenSnapshot,
    /// A single control from the semantic screen.
    Control,
    /// A region from the semantic screen.
    Region,
    /// A canonical frame (re-review Wave 2) captured at a sequence point.
    Frame,
    /// A before/after diff between two frames.
    Diff,
    /// An on-disk artifact (screenshot, recording, log, screenshot-as-text).
    Artifact,
    /// A `TransactionId` referencing an `InteractionTransaction`.
    Transaction,
    /// A recorded assertion run (passed or failed).
    Assertion,
    /// A captured terminal event (bell, title, exit, etc.).
    TerminalEvent,
    /// Free-form evidence (use sparingly; prefer the typed variants).
    Other,
}

/// A single piece of evidence attached to a [`Finding`] (re-review Part XV).
///
/// One Finding can carry `Vec<EvidenceRef>`. Each ref is a citable link
/// back to the runtime data that produced the finding: a specific frame
/// hash, control id, transaction id, or on-disk artifact path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvidenceRef {
    /// What this evidence IS.
    pub kind: EvidenceKind,
    /// Optional target id: control_id, region_id, frame_id, transaction_id,
    /// or structure_hash for a screen snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Short human-readable label, e.g. "Save button at (40,12)".
    pub summary: String,
    /// Kind-specific structured detail. Free-form but typed (JSON object
    /// recommended so JSON Schema validation can enforce keys per kind).
    pub detail: serde_json::Value,
    /// Optional on-disk artifact path (screenshot, recording, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<PathBuf>,
}

impl EvidenceRef {
    /// Convenience constructor for the common "point at something by id" case.
    pub fn point(
        kind: EvidenceKind,
        target: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        EvidenceRef {
            kind,
            target: Some(target.into()),
            summary: summary.into(),
            detail: serde_json::Value::Null,
            artifact: None,
        }
    }

    /// Convenience constructor for a screen-snapshot evidence ref.
    pub fn screen(structure_hash: impl Into<String>, summary: impl Into<String>) -> Self {
        EvidenceRef::point(EvidenceKind::ScreenSnapshot, structure_hash, summary)
    }

    /// Convenience constructor for a control evidence ref.
    pub fn control(control_id: impl Into<String>, summary: impl Into<String>) -> Self {
        EvidenceRef::point(EvidenceKind::Control, control_id, summary)
    }

    /// Convenience constructor for a region evidence ref.
    pub fn region(region_id: impl Into<String>, summary: impl Into<String>) -> Self {
        EvidenceRef::point(EvidenceKind::Region, region_id, summary)
    }

    /// Convenience constructor for a transaction evidence ref.
    pub fn transaction(transaction_id: impl Into<String>, summary: impl Into<String>) -> Self {
        EvidenceRef::point(EvidenceKind::Transaction, transaction_id, summary)
    }

    /// Convenience constructor for an on-disk artifact evidence ref.
    pub fn artifact(path: PathBuf, summary: impl Into<String>) -> Self {
        EvidenceRef {
            kind: EvidenceKind::Artifact,
            target: None,
            summary: summary.into(),
            detail: serde_json::Value::Null,
            artifact: Some(path),
        }
    }

    /// Attach structured detail, returning `self` for fluent construction.
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

/// One audit finding. `evidence` is a list of [`EvidenceRef`]s (re-review
/// Part XV) — a Finding can carry many citations, not just one inline blob.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: String, // error | warn | info
    pub category: String,
    pub summary: String,
    /// One or more typed evidence references. Always non-empty for a
    /// well-formed finding; empty evidence is treated as a bug.
    pub evidence: Vec<EvidenceRef>,
    pub confidence: f32,
}

impl Finding {
    /// Build a finding with a single screen-snapshot evidence ref.
    pub fn with_screen(
        id: impl Into<String>,
        severity: impl Into<String>,
        category: impl Into<String>,
        summary: impl Into<String>,
        screen_hash: impl Into<String>,
        confidence: f32,
    ) -> Self {
        let s: String = summary.into();
        let hash: String = screen_hash.into();
        Finding {
            id: id.into(),
            severity: severity.into(),
            category: category.into(),
            summary: s.clone(),
            evidence: vec![EvidenceRef::screen(hash, s)],
            confidence,
        }
    }
}

/// Run the requested audit profile and return evidence-backed findings.
pub fn run(profile: &str, screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let want = |p: &str| profile == "full" || profile == p;
    if want("focus") {
        out.extend(static_focus_audit(screen, sem));
    }
    if want("layout") || want("clipping") {
        out.extend(static_clipping_audit(screen, sem));
    }
    if want("discoverability") {
        out.extend(discoverability_audit(screen, sem));
    }
    if want("keyboard") {
        out.extend(static_keyboard_audit(sem));
    }
    if want("navigation") {
        out.push(Finding {
            id: "NAV-INFO".into(),
            severity: "info".into(),
            category: "navigation".into(),
            summary: "Navigation graph is built incrementally by tui_explore; static graph not available from a single frame.".into(),
            evidence: vec![EvidenceRef::screen(
                screen.structure_hash.clone(),
                "static navigation graph not derivable from a single frame",
            )],
            confidence: 1.0,
        });
    }
    if want("color") || want("performance") || want("mouse") || want("states") || want("errors") {
        let profile_name = profile.to_string();
        let summary = format!(
            "Profile '{}' static checks not yet implemented; structure available via tui_observe mode=semantic.",
            profile_name
        );
        out.push(Finding {
            id: format!("{}-INFO", profile_name.to_uppercase()),
            severity: "info".into(),
            category: profile_name.clone(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), summary)
                .with_detail(json!({
                    "note": "static profile not implemented; use tui_observe mode=semantic",
                    "profile": profile_name,
                }))],
            confidence: 1.0,
        });
    }
    out
}

fn static_focus_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let reverse_count = screen.cells.iter().filter(|c| c.reverse).count();
    if sem.focus.control.is_none() && reverse_count == 0 {
        let summary = "No detectable focus target on this screen.".to_string();
        out.push(Finding {
            id: "FOCUS-001".into(),
            severity: "warn".into(),
            category: "focus".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), summary)
                .with_detail(json!({
                    "reverse_cells": reverse_count,
                    "cursor": screen.cursor,
                }))],
            confidence: 0.7,
        });
    } else if sem.focus.control.is_some() {
        let ctrl = sem.focus.control.clone().unwrap_or_default();
        let summary = format!(
            "Focus on '{}' (confidence {:.2})",
            ctrl, sem.focus.confidence
        );
        out.push(Finding {
            id: "FOCUS-OK".into(),
            severity: "info".into(),
            category: "focus".into(),
            summary: summary.clone(),
            evidence: vec![
                EvidenceRef::control(ctrl.clone(), summary).with_detail(json!({
                    "evidence": sem.focus.evidence,
                })),
            ],
            confidence: sem.focus.confidence,
        });
    }
    out
}

fn static_clipping_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    for rg in &sem.regions {
        let b = &rg.bounds;
        if (b.x + b.width) > screen.cols || (b.y + b.height) > screen.rows {
            let summary = format!("Region '{}' extends beyond terminal bounds.", rg.id);
            out.push(Finding {
                id: "CLIP-001".into(),
                severity: "error".into(),
                category: "clipping".into(),
                summary: summary.clone(),
                evidence: vec![
                    EvidenceRef::region(rg.id.clone(), summary).with_detail(json!({
                        "bounds": b,
                        "terminal": { "cols": screen.cols, "rows": screen.rows },
                    })),
                ],
                confidence: 0.96,
            });
        }
    }
    out
}

/// Discoverability audit over semantic affordances (re-review: the old grep
/// for the substring "help"/"ctrl+" could not distinguish "observed
/// `d → delete`" from "no visible hint" — a destructive action with no cue
/// is a finding, not a non-event).
fn discoverability_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let keybinds: Vec<&Affordance> = sem
        .affordances
        .iter()
        .filter(|a| matches!(a.invocation, Invocation::Key { .. }))
        .collect();
    let labeled: Vec<&Affordance> = keybinds
        .iter()
        .copied()
        .filter(|a| a.visibility == Visibility::Labeled)
        .collect();

    if keybinds.is_empty() {
        let summary = "No keyboard affordances detected on this screen.".to_string();
        out.push(Finding {
            id: "DISC-001".into(),
            severity: "warn".into(),
            category: "discoverability".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), summary)
                .with_detail(json!({
                    "note": "some TUIs hide hints intentionally; confirm against design contract",
                }))],
            confidence: 0.5,
        });
        return out;
    }

    let summary = format!(
        "{} keyboard affordance(s), {} with on-screen hints.",
        keybinds.len(),
        labeled.len()
    );
    out.push(Finding {
        id: "DISC-OK".into(),
        severity: "info".into(),
        category: "discoverability".into(),
        summary: summary.clone(),
        evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), &summary).with_detail(
            json!({
                "labeled": labeled
                    .iter()
                    .filter_map(|a| match &a.invocation {
                        Invocation::Key { key } => Some(json!({
                            "action": a.action,
                            "key": key,
                        })),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                "hint_lines": labeled.iter().filter_map(|a| a.hint_text.clone()).collect::<Vec<_>>(),
            }),
        )],
        confidence: 0.85,
    });

    // Hidden-keybinding heuristic: prose words that commonly announce
    // undocumented destructive/quit bindings ("type d to delete") when the
    // affordance list has no labeled cue for them.
    let joined = screen.viewport_text.join("\n").to_lowercase();
    for verb in ["delete", "remove", "quit", "kill"] {
        let pattern = format!("to {}", verb);
        if joined.contains(&pattern)
            && !labeled
                .iter()
                .any(|a| a.action.to_lowercase().contains(verb))
        {
            let s = format!(
                "Screen suggests a '{}' action but no labeled key hint was found for it.",
                verb
            );
            out.push(Finding {
                id: format!("DISC-HIDDEN-{}", verb.to_uppercase()),
                severity: "warn".into(),
                category: "discoverability".into(),
                summary: s.clone(),
                evidence: vec![
                    EvidenceRef::screen(screen.structure_hash.clone(), &s).with_detail(
                        json!({ "note": "destructive/quit action may be undiscoverable" }),
                    ),
                ],
                confidence: 0.6,
            });
        }
    }
    out
}

fn static_keyboard_audit(sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let buttons: Vec<&Control> = sem
        .controls
        .iter()
        .filter(|c| c.kind == ControlKind::Button)
        .collect();
    if !buttons.is_empty() {
        let summary = format!(
            "Detected {} button-like control(s); verify Tab/Enter reachability via tui_explore.",
            buttons.len()
        );
        let button_labels: Vec<String> = buttons.iter().map(|b| b.label.clone()).collect();
        out.push(Finding {
            id: "KB-INFO".into(),
            severity: "info".into(),
            category: "keyboard".into(),
            summary: summary.clone(),
            evidence: vec![
                EvidenceRef::point(EvidenceKind::Other, "keyboard_buttons", summary).with_detail(
                    json!({
                        "controls": button_labels,
                    }),
                ),
            ],
            confidence: 0.85,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: Vec<&str>) -> ScreenState {
        ScreenState {
            cols: 80,
            rows: rows.len() as u16,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows.into_iter().map(String::from).collect(),
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: crate::screen::ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    /// The review's motivating contrast: a hint footer yields labeled
    /// affordances (no warning); a bare screen yields DISC-001.
    #[test]
    fn discoverability_contrasts_hints_vs_bare() {
        let with_hints = screen(vec!["Item list", "q quit  ^X exit"]);
        let sem = crate::semantic::analyze(&with_hints);
        let f = discoverability_audit(&with_hints, &sem);
        assert!(
            f.iter().any(|x| x.id == "DISC-OK"),
            "labeled hints produce the info finding"
        );
        assert!(!f.iter().any(|x| x.id == "DISC-001"));

        let bare = screen(vec!["Just a title line", "and body text"]);
        let sem_bare = crate::semantic::analyze(&bare);
        let f2 = discoverability_audit(&bare, &sem_bare);
        assert!(
            f2.iter().any(|x| x.id == "DISC-001"),
            "no affordances → warn finding"
        );
    }

    /// A destructive verb mentioned without any labeled key hint is flagged
    /// as potentially undiscoverable (the "d → delete, no cue" case).
    #[test]
    fn destructive_action_without_hint_is_flagged() {
        let s = screen(vec![
            "Records loaded.",
            "Type d to delete the selected record.",
        ]);
        let sem = crate::semantic::analyze(&s);
        let f = discoverability_audit(&s, &sem);
        assert!(
            f.iter().any(|x| x.id == "DISC-HIDDEN-DELETE"),
            "destructive-without-cue must be flagged: {:?}",
            f.iter().map(|x| x.id.clone()).collect::<Vec<_>>()
        );

        // With a labeled hint present, the same verb is not flagged.
        let s2 = screen(vec![
            "Type d to delete the selected record.",
            "d delete  q quit",
        ]);
        let sem2 = crate::semantic::analyze(&s2);
        let f2 = discoverability_audit(&s2, &sem2);
        assert!(
            !f2.iter().any(|x| x.id == "DISC-HIDDEN-DELETE"),
            "labeled hint for delete suppresses the finding"
        );
    }

    /// `run()` end-to-end still works with the reshaped semantic screen
    /// (affordances field present).
    #[test]
    fn full_profile_runs() {
        let s = screen(vec!["[ OK ]", "q quit"]);
        let sem = crate::semantic::analyze(&s);
        let findings = run("full", &s, &sem);
        assert!(
            findings.iter().any(|f| f.category == "discoverability"),
            "discoverability runs in the full profile"
        );
    }
}
