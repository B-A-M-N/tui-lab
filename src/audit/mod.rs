//! UX audit engine: deterministic rules + evidence (spec section 14-21).
//! The LLM (Hermes) may interpret results afterward; the engine only emits
//! findings with evidence.

use crate::screen::ScreenState;
use crate::semantic::{Affordance, Control, ControlKind, Invocation, SemanticScreen, Visibility};
use serde_json::json;
use std::path::PathBuf;

pub mod compare;
pub mod repair;
pub mod driver;
pub mod orchestrator;
pub mod transaction;
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
    /// Instance-unique id (re-review P1: three clipped regions used to all
    /// read `CLIP-001`, making every downstream citation ambiguous). The
    /// rule is preserved in [`Self::rule_id`]; the instance id is the
    /// rule plus a stable target/context discriminator assigned by
    /// [`Finding::instance`].
    pub id: String,
    /// The audit rule that produced this finding (`CLIP-001`,
    /// `FOCUS-001`, …) — stable across runs, the comparison key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub severity: String, // error | warn | info
    pub category: String,
    pub summary: String,
    /// One or more typed evidence references. Always non-empty for a
    /// well-formed finding; empty evidence is treated as a bug.
    pub evidence: Vec<EvidenceRef>,
    pub confidence: f32,
    /// Reproducible form (Wave D item 38): the scenario ID of a minimized,
    /// replayable reproduction saved into the run. `None` for findings that
    /// are not reproductions (most static findings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<String>,
    /// Probable source loci for the problem (W2.10): empty for purely
    /// screen-derived findings; populated when evidence can name a file:line
    /// (native coverage events, framework adapters, stack-derived loci).
    /// `SourceRef::is_actionable()` gates whether a repair pass should
    /// trust the locus — a low-confidence guess is carried, not hidden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<crate::semantic::source_ref::SourceRef>,
}

impl Finding {
    /// Split rule id + instance discriminator (re-review P1 item 31): the
    /// instance id is unique per (rule, semantic target) pair, stable for
    /// the same target on the same rule — comparisons fingerprint rule +
    /// target, and two distinct clipped regions can never collide.
    pub fn instance(mut self) -> Self {
        let rule = self.rule_id.clone().unwrap_or_else(|| self.id.clone());
        // Discriminator: the first evidence target (region/control id,
        // screen hash, …) — the semantic target of the finding.
        let target = self
            .evidence
            .first()
            .map(|e| e.target.clone())
            .unwrap_or_default();
        if self.rule_id.is_none() {
            self.rule_id = Some(rule.clone());
        }
        if target.as_deref().map(str::is_empty).unwrap_or(true) {
            self.id = rule;
        } else {
            self.id = format!("{rule}@{}", short_hash(&rule, &target.unwrap_or_default()));
        }
        self
    }
}

/// Stable 8-hex discriminator for a (rule, target) pair.
fn short_hash(rule: &str, target: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    rule.hash(&mut h);
    target.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
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
            rule_id: None,
            severity: severity.into(),
            category: category.into(),
            summary: s.clone(),
            evidence: vec![EvidenceRef::screen(hash, s)],
            confidence,
            reproduction: None,
            source_refs: Vec::new(),
        }
    }

    /// Attach probable source loci, returning the finding (chainable).
    pub fn with_source_refs(
        mut self,
        refs: Vec<crate::semantic::source_ref::SourceRef>,
    ) -> Self {
        self.source_refs = refs;
        self
    }
}

/// Run the requested audit profile and return evidence-backed findings.
pub fn run(
    profile: &str,
    screen: &ScreenState,
    sem: &SemanticScreen,
) -> anyhow::Result<Vec<Finding>> {
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
    // Re-review item 44: profiles with NO static pass are caller errors,
    // never fabricated `info` rows. This static entry point only accepts
    // profiles it can actually check; the active profiles (color,
    // performance, mouse, states, errors, navigation, contract) run
    // through the orchestrator's real drivers instead.
    if !static_profile_available(profile) {
        return Err(anyhow::anyhow!(
            "profile '{}' has no static checks; use an audit action that drives \
             the app (probe/explore/orchestrator) or a profile with static checks \
             (focus, layout, discoverability, keyboard)",
            profile
        ));
    }
    Ok(out)
}

/// Whether `profile` is a static profile this build can actually run.
/// The MCP audit surface validates with this instead of discovering the
/// gap in the results (item 44: no placeholder findings).
pub fn static_profile_available(profile: &str) -> bool {
    matches!(profile, "full" | "focus" | "layout" | "clipping" | "discoverability" | "keyboard")
}

fn static_focus_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let mut out = Vec::new();
    let reverse_count = screen.cells.iter().filter(|c| c.reverse).count();
    if sem.focus.control.is_none() && reverse_count == 0 {
        let summary = "No detectable focus target on this screen.".to_string();
        out.push(Finding {
            id: "FOCUS-001".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "focus".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), summary)
                .with_detail(json!({
                    "reverse_cells": reverse_count,
                    "cursor": screen.cursor,
                }))],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else if sem.focus.control.is_some() {
        let ctrl = sem.focus.control.clone().unwrap_or_default();
        let summary = format!(
            "Focus on '{}' (confidence {:.2})",
            ctrl, sem.focus.confidence
        );
        out.push(Finding {
            id: "FOCUS-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "focus".into(),
            summary: summary.clone(),
            evidence: vec![
                EvidenceRef::control(ctrl.clone(), summary).with_detail(json!({
                    "evidence": sem.focus.evidence,
                })),
            ],
            confidence: sem.focus.confidence,
            reproduction: None,
            source_refs: Vec::new(),
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
                rule_id: None,
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
                reproduction: None,
                source_refs: Vec::new(),
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
            rule_id: None,
            severity: "warn".into(),
            category: "discoverability".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::screen(screen.structure_hash.clone(), summary)
                .with_detail(json!({
                    "note": "some TUIs hide hints intentionally; confirm against design contract",
                }))],
            confidence: 0.5,
            reproduction: None,
            source_refs: Vec::new(),
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
        rule_id: None,
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
        reproduction: None,
        source_refs: Vec::new(),
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
                rule_id: None,
                severity: "warn".into(),
                category: "discoverability".into(),
                summary: s.clone(),
                evidence: vec![
                    EvidenceRef::screen(screen.structure_hash.clone(), &s).with_detail(
                        json!({ "note": "destructive/quit action may be undiscoverable" }),
                    ),
                ],
                confidence: 0.6,
                reproduction: None,
                source_refs: Vec::new(),
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
            rule_id: None,
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
            reproduction: None,
            source_refs: Vec::new(),
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
        let findings = run("full", &s, &sem).expect("full is static-available");
        assert!(
            findings.iter().any(|f| f.category == "discoverability"),
            "discoverability runs in the full profile"
        );
        // Item 44: a profile without static checks is a caller error, not
        // a fabricated info finding.
        assert!(run("mouse", &s, &sem).is_err());
        assert!(static_profile_available("focus"));
        assert!(!static_profile_available("mouse"));
    }
}
