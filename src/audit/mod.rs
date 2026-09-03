//! UX audit engine: deterministic rules + evidence (spec section 14-21).
//! The LLM (Hermes) may interpret results afterward; the engine only emits
//! findings with evidence.

use crate::screen::ScreenState;
use crate::semantic::{Affordance, Control, ControlKind, Invocation, SemanticScreen, Visibility};
use serde_json::json;
use std::path::PathBuf;

pub mod compare;
pub mod driver;
pub mod orchestrator;
pub mod repair;
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

/// Schema prefix for finding-instance identity hashes.
const FINDING_SCHEMA: &str = "finding:v1";

/// Stable 8-hex discriminator for a (rule, target) pair, versioned BLAKE3.
/// Mirrors the identity scheme in `state_graph.rs`: a canonical prefix +
/// length-prefixed, ordered parts, truncated to 8 hex chars so the
/// `{rule}@{hash8}` shape is preserved.
fn short_hash(rule: &str, target: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(FINDING_SCHEMA.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(b"discriminator");
    hasher.update(&[0u8]);
    hasher.update(&(rule.len() as u64).to_le_bytes());
    hasher.update(rule.as_bytes());
    hasher.update(&(target.len() as u64).to_le_bytes());
    hasher.update(target.as_bytes());
    // First 4 bytes of the digest as the discriminator — the same 8-hex
    // shape the old SipHash produced. (Parsing the 64-char hex back as a
    // u32 always overflows, which would collapse every finding to
    // 00000000 — the discriminator must come from the digest bytes.)
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    format!("{:08x}", word)
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
    pub fn with_source_refs(mut self, refs: Vec<crate::semantic::source_ref::SourceRef>) -> Self {
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
    // Wave-3 subsystem audits (static): unicode safety and control/region
    // coverage. Deterministic reads of one fused frame.
    if want("unicode") {
        out.extend(unicode_audit(screen));
    }
    if want("controls") {
        out.extend(controls_audit(screen, sem));
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
             (focus, layout, discoverability, keyboard, unicode, controls)",
            profile
        ));
    }
    Ok(out)
}

/// Whether `profile` is a static profile this build can actually run.
/// The MCP audit surface validates with this instead of discovering the
/// gap in the results (item 44: no placeholder findings).
pub fn static_profile_available(profile: &str) -> bool {
    matches!(
        profile,
        "full"
            | "focus"
            | "layout"
            | "clipping"
            | "discoverability"
            | "keyboard"
            | "unicode"
            | "controls"
    )
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

/// Wave-3 (unicode subsystem): the frame's unicode safety. The two failure
/// classes the terminal-grid model cares about:
/// 1. **Wide glyphs** — a double-width char (CJK, emoji) whose continuation
///    column is NOT an empty spacer cell means the app wrote a narrow
///    neighbor ON TOP of the glyph's second half: the column arithmetic the
///    whole harness relies on (click coordinates, bounds, clipping math)
///    is off by one from there to the end of the row.
/// 2. **Unrenderable control bytes** — C0/C1 bytes that leaked through as
///    literal cell text render as garbage or nothing at all; the app's
///    output encoding is broken and any test asserting on that text will
///    chase a phantom.
fn unicode_audit(screen: &ScreenState) -> Vec<Finding> {
    let mut out = Vec::new();

    // 1) Wide glyphs with a non-empty continuation cell.
    let mut wide_overlaps: Vec<(u16, u16)> = Vec::new();
    for cell in &screen.cells {
        let w = crate::screen::cell_string::display_width(&cell.text);
        if w <= 1 {
            continue;
        }
        // The continuation column(s) sit to the glyph's right on the same row.
        for cont in 1..w {
            let cx = cell.x + cont;
            if cx >= screen.cols {
                // Glyph whose second half falls off the screen edge is its
                // own truncation finding.
                wide_overlaps.push((cell.x, cell.y));
                break;
            }
            if let Some(neighbor) = screen.cells.iter().find(|c| c.x == cx && c.y == cell.y) {
                if !neighbor.text.is_empty() {
                    wide_overlaps.push((cell.x, cell.y));
                    break;
                }
            }
        }
    }
    if !wide_overlaps.is_empty() {
        let first = wide_overlaps[0];
        let summary = format!(
            "{} double-width glyph(s) overlap a following cell — column arithmetic is unreliable from the first overlap onward.",
            wide_overlaps.len()
        );
        out.push(Finding {
            id: "UNI-WIDE".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "unicode".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "wide_glyph_overlap",
                summary,
            )
            .with_detail(json!({
                "count": wide_overlaps.len(),
                "first": { "x": first.0, "y": first.1 },
                "note": "click coordinates and bounds west of the overlap stay exact; east of it they are off by the accumulated width",
            }))],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // 2) Control bytes that leaked into cell text.
    let leaked: Vec<(u16, u16)> = screen
        .cells
        .iter()
        .filter(|c| c.text.chars().any(|ch| ch.is_control() && ch != '\t'))
        .map(|c| (c.x, c.y))
        .collect();
    if !leaked.is_empty() {
        let summary = format!(
            "{} cell(s) contain literal control characters — raw bytes leaked through the app's output encoding.",
            leaked.len()
        );
        out.push(Finding {
            id: "UNI-CTRL".into(),
            rule_id: None,
            severity: "warn".into(),
            category: "unicode".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "control_char_in_cell",
                summary,
            )
            .with_detail(json!({
                "count": leaked.len(),
                "first": leaked.first(),
            }))],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    out
}

/// Wave-3 (control/region coverage): structural coverage gaps a single
/// fused frame can prove:
/// 1. **Unclaimed controls** — a control outside every detected region
///    usually means the region detection missed the panel that owns it
///    (or the control is drawn at a raw position the app never declared);
///    region-scoped operations (assert region=, clipping math) silently
///    skip it.
/// 2. **Region-overlapping controls** — a control claimed by 2+ regions is
///    an ambiguous parent; `region:` scoping cannot resolve which panel
///    owns it, and a layout change can silently move it between them.
fn controls_audit(screen: &ScreenState, sem: &SemanticScreen) -> Vec<Finding> {
    let _ = screen;
    let mut out = Vec::new();

    fn region_contains(r: &crate::semantic::regions::Region, c: &Control) -> bool {
        let b = &r.bounds;
        // A control is claimed by a region when its label start sits inside
        // the region's bounds (the same join the focus/relationship engines
        // use for single-line controls).
        c.bounds.x >= b.x
            && c.bounds.x < b.x + b.width.max(1)
            && c.bounds.y >= b.y
            && c.bounds.y < b.y + b.height.max(1)
    }

    let unclaimed: Vec<&Control> = sem
        .controls
        .iter()
        .filter(|c| !sem.regions.iter().any(|r| region_contains(r, c)))
        .collect();
    if !unclaimed.is_empty() {
        let labels: Vec<String> = unclaimed.iter().map(|c| c.label.clone()).collect();
        let summary = format!(
            "{} control(s) sit outside every detected region: {}.",
            unclaimed.len(),
            labels.join(", ")
        );
        out.push(Finding {
            id: "CTRL-ORPHAN".into(),
            rule_id: None,
            severity: "info".into(),
            category: "controls".into(),
            summary: summary.clone(),
            evidence: vec![EvidenceRef::point(
                EvidenceKind::Other,
                "unclaimed_controls",
                summary,
            )
            .with_detail(json!({
                "controls": labels,
                "note": "region-scoped operations skip these; either region detection missed the panel or the control floats outside any panel",
            }))],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    for c in &sem.controls {
        let owners: Vec<&crate::semantic::regions::Region> = sem
            .regions
            .iter()
            .filter(|r| region_contains(r, c))
            .collect();
        if owners.len() > 1 {
            let ids: Vec<String> = owners.iter().map(|r| r.id.clone()).collect();
            let summary = format!(
                "Control '{}' is claimed by {} regions ({}).",
                c.label,
                owners.len(),
                ids.join(", ")
            );
            out.push(Finding {
                id: "CTRL-AMBIG".into(),
                rule_id: None,
                severity: "warn".into(),
                category: "controls".into(),
                summary: summary.clone(),
                evidence: vec![EvidenceRef::point(
                    EvidenceKind::Other,
                    "ambiguous_region_parent",
                    summary,
                )
                .with_detail(json!({
                    "control": c.label,
                    "regions": ids,
                }))],
                confidence: 0.8,
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

    /// Wave-3 (unicode): a wide glyph followed by a NON-empty cell means
    /// something was written over the glyph's continuation column — the
    /// column arithmetic every consumer trusts is off from there eastward.
    #[test]
    fn unicode_audit_detects_wide_overlap_and_control_leak() {
        use crate::screen::Cell;
        let mut s = screen(vec!["界A"]);
        s.cols = 8;
        // Manually lay cells: 界 at (0,0) with continuation (1,0), then 'A'
        // written at (1,0) ON TOP of the continuation — the overlap case.
        s.cells = vec![
            Cell {
                x: 0,
                y: 0,
                text: "界".into(),
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            },
            Cell {
                x: 1,
                y: 0,
                text: "A".into(),
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            },
        ];
        let findings = unicode_audit(&s);
        assert!(
            findings.iter().any(|f| f.id == "UNI-WIDE"),
            "wide overlap detected: {:?}",
            findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>()
        );

        // A correct wide row (empty continuation) produces no finding.
        let mut ok = screen(vec!["界A"]);
        ok.cols = 8;
        ok.cells = vec![
            Cell {
                x: 0,
                y: 0,
                text: "界".into(),
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            },
            Cell {
                x: 1,
                y: 0,
                text: String::new(),
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            },
            Cell {
                x: 2,
                y: 0,
                text: "A".into(),
                fg: crate::screen::Color::unknown(),
                bg: crate::screen::Color::unknown(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strike: false,
            },
        ];
        assert!(
            unicode_audit(&ok).is_empty(),
            "correct wide layout is clean"
        );

        // A literal control byte in a cell leaks through the encoding.
        let mut leak = screen(vec!["a\x07b"]);
        leak.cells = vec![Cell {
            x: 0,
            y: 0,
            text: "a\u{7}b".into(),
            fg: crate::screen::Color::unknown(),
            bg: crate::screen::Color::unknown(),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strike: false,
        }];
        assert!(
            unicode_audit(&leak).iter().any(|f| f.id == "UNI-CTRL"),
            "control leak detected"
        );
    }

    /// Wave-3 (controls): a control outside every region is an orphan
    /// (region-scoped ops skip it); one claimed by two regions is an
    /// ambiguous parent.
    #[test]
    fn controls_audit_flags_orphans_and_ambiguous_parents() {
        use crate::semantic::controls::{Control, ControlBounds, ControlKind};
        use crate::semantic::regions::Region;

        let s = screen(vec!["[ Save ]"]);
        let control = Control {
            id: "btn/save".into(),
            kind: ControlKind::Button,
            label: "Save".into(),
            value: None,
            bounds: ControlBounds {
                x: 1,
                y: 0,
                width: 4,
                height: 1,
            },
            region_id: None,
            focusable: true,
            focused: false,
            enabled: true,
            selected: false,
            checked: false,
            shortcut: None,
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
            evidence: vec![],
            source: "inferred".into(),
        };
        let mk_region = |id: &str, x: u16, w: u16| Region {
            id: id.into(),
            kind: crate::semantic::regions::RegionKind::Panel,
            title: None,
            bounds: crate::semantic::regions::Bounds {
                x,
                y: 0,
                width: w,
                height: 1,
            },
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
            parent_id: None,
            child_ids: vec![],
            clipping_state: crate::semantic::regions::ClippingState::None,
        };
        let sem = crate::semantic::SemanticScreen {
            cols: s.cols,
            rows: s.rows,
            regions: vec![mk_region("left", 0, 8), mk_region("right", 1, 12)],
            controls: vec![control.clone()],
            focus: Default::default(),
            relationships: vec![],
            affordances: vec![],
            components: vec![],
        };
        let findings = controls_audit(&s, &sem);
        assert!(
            findings.iter().any(|f| f.id == "CTRL-AMBIG"),
            "two overlapping regions both claim the control: {:?}",
            findings
        );

        // Orphan: no region covers x=1.
        let sem2 = crate::semantic::SemanticScreen {
            regions: vec![mk_region("far", 60, 10)],
            ..sem.clone()
        };
        let findings2 = controls_audit(&s, &sem2);
        assert!(
            findings2.iter().any(|f| f.id == "CTRL-ORPHAN"),
            "control outside every region is an orphan: {:?}",
            findings2
        );
    }
}
