//! Control inference: buttons, fields, checkboxes, radios (spec section 9).
//!
//! Fixes per audit item 24:
//!   * Checkbox `[x]` no longer also seen as Button("x") — toggle recognition
//!     runs first and marks the span consumed.
//!   * Parentheses `(text)` no longer auto-classified as Button — only `[ ]`,
//!     `< >` are buttons; `( )` is reserved for radio toggles.
//!   * Field extraction skips colons inside bordered regions (│ Host: localhost │)
//!     by checking that the label doesn't start with border glyphs.
//!   * Field x-coordinate is computed from the actual label start, not hardcoded 0.
//!   * Labels like "12:43" (time) or "http://" (URL) are not treated as fields.

use crate::screen::ScreenState;
use crate::semantic::confidence::Confidence;
use crate::semantic::recognizers;
use crate::semantic::regions::Region;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Button,
    Field,
    Checkbox,
    Radio,
    Tab,
    List,
    MenuItem,
    Status,
    Progress,
    Spinner,
    Label,
    Unknown,
}

/// A detected UI control.
///
/// Expanded per audit item 27 to include stable IDs, spatial bounds,
/// state flags, and interaction metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Control {
    /// Stable ID derived from role, label, and geometry.
    /// Format: `{kind}:{sanitized_label}:{x},{y}`
    pub id: String,
    pub kind: ControlKind,
    pub label: String,
    /// For fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Bounds (x, y, width, height) in character cells.
    pub bounds: ControlBounds,
    /// Region this control belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_id: Option<String>,
    /// Can this control receive focus?
    #[serde(default)]
    pub focusable: bool,
    /// Currently focused (reverse video / cursor on this control).
    #[serde(default)]
    pub focused: bool,
    /// Is this control enabled (not grayed out)?
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Selected state (for tabs, list items).
    #[serde(default)]
    pub selected: bool,
    /// Checked state (for checkboxes, radio buttons).
    #[serde(default)]
    pub checked: bool,
    /// Keyboard shortcut (e.g., "Ctrl+S").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    pub confidence: Confidence,
    /// Evidence for this detection.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Source: "inferred" for heuristics, "native" for framework adapter.
    #[serde(default = "default_source")]
    pub source: String,
}

/// Bounding box in character cells.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ControlBounds {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

fn default_true() -> bool {
    true
}
fn default_source() -> String {
    "inferred".to_string()
}

/// Check if point (px, py) is within bounds [x, x+width) x [y, y+height).
fn region_contains_point(bounds: &crate::semantic::regions::Bounds, px: u16, py: u16) -> bool {
    px >= bounds.x
        && py >= bounds.y
        && px < bounds.x + bounds.width
        && py < bounds.y + bounds.height
}

/// Slugify a label into an ID path segment (lowercase alphanumeric + hyphen,
/// capped at 20 chars).
fn slugify_label(label: &str) -> String {
    let cleaned: String = label
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed: String = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let truncated: String = collapsed.chars().take(20).collect();
    let truncated = truncated.trim_end_matches('-').to_string();
    if truncated.is_empty() {
        "unnamed".to_string()
    } else {
        truncated
    }
}

/// Generate a stable, geometry-free control ID (re-review Wave-3 item 16 —
/// `{kind}:{label}:{x},{y}` made every resize a new control, breaking focus
/// tracking, scenario replay, and audit cross-references).
///
/// Format: `{kind}/{label-slug}` optionally prefixed with the containing
/// region's stable path: `dialog/settings/button/save`. Duplicates (same
/// region path + kind + label) get a 1-based `#{n}` disambiguator appended
/// by the caller's second pass, since duplicates are only knowable after
/// every control on the screen has been collected.
fn stable_id(kind: &ControlKind, label: &str, region_path: Option<&str>) -> String {
    let kind_str = slugify_label(&format!("{:?}", kind));
    let label_slug = slugify_label(label);
    match region_path {
        Some(rp) if !rp.is_empty() => format!("{}/{}/{}", rp, kind_str, label_slug),
        _ => format!("{}/{}", kind_str, label_slug),
    }
}

/// Disambiguate duplicate control IDs with `#{n}` suffixes (second pass —
/// duplicates are only knowable once the whole screen is collected).
fn disambiguate_control_ids(controls: &mut [Control]) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for ctrl in controls.iter_mut() {
        let n = seen.entry(ctrl.id.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            ctrl.id = format!("{}#{}", ctrl.id, n);
        }
    }
}

/// Detect button-like `[ Label ]` / `< Label >` tokens, field `Label:` marks,
/// and checkbox/radio glyphs.
///
/// Detection order per line:
///   toggle → button → field → tabs → menu → list → status → progress → spinner
///
/// Once a span is consumed by an earlier recognizer it is not reconsidered.
pub fn detect_controls(screen: &ScreenState, regions: &[Region]) -> Vec<Control> {
    let mut out = Vec::new();

    for (y, line) in screen.viewport_text.iter().enumerate() {
        let y = y as u16;
        let mut consumed: Vec<(u16, u16)> = mark_consumed_spans(line, regions, y);
        // Menu/status suppression is per-ROW (re-review Wave-3 item 20): a
        // button or field on this line means the line is a control row, not
        // a menu bar or status readout. The old screen-global `any(...)` let
        // a single button anywhere suppress menu recognition everywhere.
        let line_has_button_or_field = extract_bracketed(line)
            .iter()
            .any(|cap| !is_toggle_text(&cap.text))
            || extract_field(line).is_some();

        // 1) checkbox/radio: ( ) or (*) or [ ] or [x]
        for cb in extract_toggle(line) {
            if overlaps_consumed(&consumed, cb.x, cb.x + 3) {
                continue;
            }
            out.push(Control {
                id: stable_id(&cb.kind, &cb.text, None),
                checked: cb.checked,
                kind: cb.kind.clone(),
                label: cb.text.clone(),
                value: None,
                bounds: ControlBounds {
                    x: cb.x,
                    y,
                    width: cb.text.chars().count() as u16,
                    height: 1,
                },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                // A `(*)` radio glyph means this option is the selected one
                // of its group; checkboxes report state through `checked`.
                selected: cb.kind == ControlKind::Radio && cb.checked,
                shortcut: None,
                confidence: Confidence::inferred(0.9, &["toggle-glyph"]),
                evidence: vec!["toggle-glyph".to_string()],
                source: "inferred".to_string(),
            });
            consumed.push((cb.x, cb.x + 3));
        }

        // 2) buttons: [ text ] or < text >
        //    Skip `[x]`, `[ ]` which are toggles, not buttons.
        for cap in extract_bracketed(line) {
            // `[ 确定 ]`: bracket + space + label + space — column arithmetic
            // needs display width, not UTF-8 bytes (item 22).
            let span_end = cap.x + cap.text.chars().count() as u16 + 2;
            if overlaps_consumed(&consumed, cap.x, span_end) {
                continue;
            }
            if is_toggle_text(&cap.text) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Button, &cap.text, None),
                kind: ControlKind::Button,
                label: cap.text.clone(),
                value: None,
                bounds: ControlBounds {
                    x: cap.x,
                    y,
                    width: cap.text.chars().count() as u16,
                    height: 1,
                },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
                checked: false,
                shortcut: None,
                confidence: Confidence::inferred(0.85, &["bracketed-label"]),
                evidence: vec!["bracketed-label".to_string()],
                source: "inferred".to_string(),
            });
            consumed.push((cap.x, span_end));
        }

        // 3) field: "Label:" optionally followed by a value
        if let Some(f) = extract_field(line) {
            let label_cols = f.label.chars().count() as u16;
            if overlaps_consumed(&consumed, f.x, f.x + label_cols) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Field, &f.label, None),
                kind: ControlKind::Field,
                label: f.label.clone(),
                value: f.value,
                bounds: ControlBounds {
                    x: f.x,
                    y,
                    width: label_cols,
                    height: 1,
                },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
                checked: false,
                shortcut: None,
                confidence: Confidence::inferred(0.85, &["label-colon"]),
                evidence: vec!["label-colon".to_string()],
                source: "inferred".to_string(),
            });
            consumed.push((f.x, f.x + label_cols));
        }

        // 4) tabs
        for cand in recognizers::detect_tabs(line, y) {
            if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                continue;
            }
            let mut ctrl = candidate_to_control(&cand, y, "inferred");
            ctrl.checked = false;
            out.push(ctrl);
            consumed.push((cand.x, cand.x + cand.width));
        }

        // 5) menu items (skip if line already has a colon-field or button)
        if !line_has_button_or_field {
            for cand in recognizers::detect_menu_items(line, y) {
                if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                    continue;
                }
                let mut ctrl = candidate_to_control(&cand, y, "inferred");
                ctrl.checked = false;
                out.push(ctrl);
                consumed.push((cand.x, cand.x + cand.width));
            }
        }

        // 6) list items
        for cand in recognizers::detect_list_items(line, y) {
            if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                continue;
            }
            let mut ctrl = candidate_to_control(&cand, y, "inferred");
            ctrl.checked = false;
            out.push(ctrl);
            consumed.push((cand.x, cand.x + cand.width));
        }

        // 7) status (skip if line already produced a Button or Field)
        if !line_has_button_or_field {
            for cand in recognizers::detect_status(line, y) {
                if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                    continue;
                }
                let mut ctrl = candidate_to_control(&cand, y, "inferred");
                ctrl.checked = false;
                out.push(ctrl);
                consumed.push((cand.x, cand.x + cand.width));
            }
        }

        // 8) progress
        for cand in recognizers::detect_progress(line, y) {
            if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                continue;
            }
            let mut ctrl = candidate_to_control(&cand, y, "inferred");
            ctrl.checked = false;
            out.push(ctrl);
            consumed.push((cand.x, cand.x + cand.width));
        }

        // 9) spinner
        for cand in recognizers::detect_spinner(line, y) {
            if overlaps_consumed(&consumed, cand.x, cand.x + cand.width) {
                continue;
            }
            let mut ctrl = candidate_to_control(&cand, y, "inferred");
            ctrl.checked = false;
            out.push(ctrl);
            consumed.push((cand.x, cand.x + cand.width));
        }
    }

    // Region linking: assign each control to the *smallest* region that
    // contains it (re-review Wave-3 item 21). Regions nest (dialog ⊂ screen
    // panel ⊂ …); taking the first hit binds controls to the outermost
    // container, which wrecks region-scoped queries like "buttons in this
    // dialog".
    for ctrl in &mut out {
        let mut best: Option<&Region> = None;
        for region in regions {
            if !region_contains_point(&region.bounds, ctrl.bounds.x, ctrl.bounds.y) {
                continue;
            }
            best = match best {
                Some(current) => {
                    let cur_area = current.bounds.width as u32 * current.bounds.height as u32;
                    let new_area = region.bounds.width as u32 * region.bounds.height as u32;
                    if new_area < cur_area {
                        Some(region)
                    } else {
                        best
                    }
                }
                None => Some(region),
            };
        }
        ctrl.region_id = best.map(|r| r.id.clone());
    }

    // Finalize stable IDs: rebuild each control's ID anchored to its
    // (now-known) containing region's stable path, then disambiguate
    // duplicates across the whole screen (re-review Wave-3 item 16).
    for ctrl in out.iter_mut() {
        let region_path = ctrl.region_id.as_deref().filter(|p| !p.is_empty());
        ctrl.id = stable_id(&ctrl.kind, &ctrl.label, region_path);
    }
    disambiguate_control_ids(&mut out);

    out
}

/// Check if text looks like a toggle glyph (so we don't also treat it as button).
fn is_toggle_text(text: &str) -> bool {
    let t = text.trim();
    t == "x" || t == "X" || t == "*" || t == " " || t.is_empty()
}

/// Track which character positions are inside bordered regions or already consumed.
fn mark_consumed_spans(line: &str, _regions: &[Region], _y: u16) -> Vec<(u16, u16)> {
    // Mark leading/trailing border glyphs as consumed so we don't parse
    // `│ Host: localhost        │` as a field with label "│ Host".
    let mut consumed = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let border_chars: [char; 17] = [
        '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╔', '╗', '╚', '╝', '║', '═',
    ];

    // Consume leading border + whitespace
    let mut i = 0;
    while i < chars.len() && (border_chars.contains(&chars[i]) || chars[i] == ' ') {
        i += 1;
    }
    if i > 0 {
        consumed.push((0, i as u16));
    }

    // Consume trailing border + whitespace
    let mut j = chars.len();
    while j > i && (border_chars.contains(&chars[j - 1]) || chars[j - 1] == ' ') {
        j -= 1;
    }
    if j < chars.len() {
        consumed.push((j as u16, chars.len() as u16));
    }

    consumed
}

/// Check if [start, end) overlaps with any consumed span.
/// Two spans overlap if they share any column.
fn overlaps_consumed(consumed: &[(u16, u16)], start: u16, end: u16) -> bool {
    consumed.iter().any(|&(cs, ce)| start < ce && end > cs)
}

/// Create a Control from a Candidate with fixed fields.
fn candidate_to_control(c: &recognizers::Candidate, y: u16, source: &str) -> Control {
    // Placeholder ID: the stable geometry-free ID is rebuilt by the
    // post-region pass at the end of detect_controls (item 16).
    let id = stable_id(&c.kind, &c.label, None);

    let ev: Vec<String> = c.evidence.iter().map(|s| s.to_string()).collect();
    let evidence_refs: Vec<&str> = c.evidence.to_vec();
    let conf = Confidence::inferred(c.confidence.score, &evidence_refs);

    Control {
        id,
        kind: c.kind.clone(),
        label: c.label.clone(),
        value: c.value.clone(),
        bounds: ControlBounds {
            x: c.x,
            y,
            width: c.width,
            height: 1,
        },
        region_id: None,
        focusable: c.focusable,
        focused: false,
        enabled: true,
        selected: c.selected,
        checked: false,
        shortcut: c.shortcut.clone(),
        confidence: conf,
        evidence: ev,
        source: source.to_string(),
    }
}

struct Span {
    text: String,
    x: u16,
}

/// Extract bracketed labels `[ text ]` or `< text >`.
/// Does NOT match `( )` — those are radio toggles.
fn extract_bracketed(line: &str) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' || c == '<' {
            let close = match c {
                '[' => ']',
                '<' => '>',
                _ => ' ',
            };
            let mut j = i + 1;
            let mut found = None;
            while j < chars.len() {
                if chars[j] == close {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(j) = found {
                if j > i + 1 {
                    let text: String = chars[(i + 1)..j].iter().collect();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        out.push(Span { text, x: i as u16 });
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

struct Toggle {
    kind: ControlKind,
    text: String,
    x: u16,
    /// Derived from the glyph itself, not the kind: `[ ]` is an *unchecked*
    /// checkbox, `[x]` checked; `( )` unselected radio, `(*)` selected
    /// (re-review Wave-3 item 18 — the old `kind == Checkbox` test marked
    /// both `[ ] Foo` and `[x] Foo` as checked).
    checked: bool,
}

fn extract_toggle(line: &str) -> Vec<Toggle> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    for i in 0..chars.len() {
        if i + 3 > chars.len() {
            continue;
        }
        let triple: String = chars[i..i + 3].iter().collect();
        let (kind, checked) = match triple.as_str() {
            "( )" => (ControlKind::Radio, false),
            "(*)" => (ControlKind::Radio, true),
            "[ ]" => (ControlKind::Checkbox, false),
            "[x]" | "[X]" => (ControlKind::Checkbox, true),
            _ => continue,
        };
        // Label runs from the glyph's end until the next toggle glyph or a
        // border glyph (re-review Wave-3 item 19): two toggles on one line
        // ("[ ] A   [x] B") must not swallow the following glyph and label
        // into the first one's text.
        let mut end = chars.len();
        let mut j = i + 3;
        while j < chars.len() {
            if j + 3 <= chars.len() {
                let next: String = chars[j..j + 3].iter().collect();
                if matches!(next.as_str(), "( )" | "(*)" | "[ ]" | "[x]" | "[X]") {
                    end = j;
                    break;
                }
            }
            if is_border_glyph(chars[j]) {
                end = j;
                break;
            }
            j += 1;
        }
        let text: String = chars[(i + 3).min(end)..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        out.push(Toggle {
            kind,
            text,
            x: i as u16,
            checked,
        });
    }
    out
}

/// Box-drawing glyphs that terminate a label span.
fn is_border_glyph(c: char) -> bool {
    matches!(
        c,
        '─' | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
            | '║'
            | '═'
    )
}

struct Field {
    label: String,
    value: Option<String>,
    x: u16,
}

/// Extract a field `Label: value` from a line.
///
/// Skip conditions:
///   * Label is empty or only digits (e.g. "12:43").
///   * Label starts with a URL-like scheme (e.g. "http://").
///   * The colon is inside a bordered region.
fn extract_field(line: &str) -> Option<Field> {
    // Find the first colon that could separate label from value.
    // Skip colons that are part of URLs (e.g. "http://") or time (e.g. "12:43").
    let chars: Vec<char> = line.chars().collect();
    let mut colon_idx = None;

    for (i, &c) in chars.iter().enumerate() {
        if c == ':' {
            // Check if this is a URL scheme colon (followed by //)
            if i + 2 < chars.len() && chars[i + 1] == '/' && chars[i + 2] == '/' {
                return None; // URL like http://, ftp://, etc.
            }
            // Check if it's a time-like pattern (digit:digit:digit)
            if i > 0 && i + 2 < chars.len() {
                let prev = chars[i - 1];
                let next1 = chars[i + 1];
                let next2 = chars[i + 2];
                if prev.is_ascii_digit() && next1.is_ascii_digit() && next2.is_ascii_digit() {
                    continue; // time like 12:43:01
                }
            }
            colon_idx = Some(i);
            break;
        }
    }

    let idx = colon_idx?;
    let before: String = chars[..idx].iter().collect();
    let after: String = chars[idx + 1..].iter().collect();

    let before_trimmed = before.trim();
    let after_trimmed = after.trim();

    // require a non-empty alphabetic-ish label
    if before_trimmed.is_empty() {
        return None;
    }
    if before_trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Skip if label starts with a border glyph
    let border_chars: [char; 17] = [
        '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╔', '╗', '╚', '╝', '║', '═',
    ];
    if before_trimmed.starts_with(|c: char| border_chars.contains(&c)) {
        return None;
    }

    let value = if after_trimmed.is_empty() {
        None
    } else {
        Some(after_trimmed.to_string())
    };

    // Find the actual x position of the label start
    let x = before.len() - before.trim_start().len();

    Some(Field {
        label: before_trimmed.to_string(),
        value,
        x: x as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_field_simple() {
        let f = extract_field("Host: localhost").unwrap();
        assert_eq!(f.label, "Host");
        assert_eq!(f.value, Some("localhost".to_string()));
        assert_eq!(f.x, 0);
    }

    #[test]
    fn test_extract_field_with_indent() {
        let f = extract_field("  Host: localhost").unwrap();
        assert_eq!(f.label, "Host");
        assert_eq!(f.value, Some("localhost".to_string()));
        assert_eq!(f.x, 2);
    }

    #[test]
    fn test_extract_field_no_value() {
        let f = extract_field("Host:").unwrap();
        assert_eq!(f.label, "Host");
        assert_eq!(f.value, None);
    }

    #[test]
    fn test_extract_field_skip_time() {
        // "12:43" should not be treated as a field
        assert!(extract_field("12:43:01").is_none());
        assert!(extract_field("Time: 12:43").is_some()); // "Time" label is ok
    }

    #[test]
    fn test_extract_field_skip_url() {
        assert!(extract_field("http://example.com").is_none());
        assert!(extract_field("URL: http://example.com").is_some()); // "URL" label is ok
    }

    #[test]
    fn test_extract_field_skip_border() {
        // Border glyphs should not be part of field label
        assert!(extract_field("│ Host: localhost │").is_none());
    }

    #[test]
    fn test_extract_bracketed_button() {
        let spans = extract_bracketed("[ Save ]");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Save");
    }

    #[test]
    fn test_extract_bracketed_angle() {
        let spans = extract_bracketed("< OK >");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "OK");
    }

    #[test]
    fn test_extract_bracketed_skip_parens() {
        // Parentheses are NOT buttons (they're radio toggles)
        let spans = extract_bracketed("( text )");
        assert_eq!(spans.len(), 0);
    }

    #[test]
    fn test_extract_toggle_checkbox() {
        let toggles = extract_toggle("[x] Option");
        assert_eq!(toggles.len(), 1);
        assert_eq!(toggles[0].kind, ControlKind::Checkbox);
        assert_eq!(toggles[0].text, "Option");
    }

    /// Wave-3 item 19: a toggle's label terminates at the next toggle glyph
    /// (or a border) — two toggles on one line must not swallow each other.
    #[test]
    fn test_toggle_label_ends_at_next_toggle() {
        let toggles = extract_toggle("[ ] Alpha   [x] Beta");
        assert_eq!(toggles.len(), 2, "both glyphs detected");
        assert_eq!(
            toggles[0].text, "Alpha",
            "first label must not contain the second glyph"
        );
        assert_eq!(toggles[1].text, "Beta");
        assert!(!toggles[0].checked);
        assert!(toggles[1].checked);
    }

    /// Item 19: a border glyph also terminates the label.
    #[test]
    fn test_toggle_label_ends_at_border() {
        let toggles = extract_toggle("[x] Done │ status");
        assert_eq!(toggles.len(), 1);
        assert_eq!(toggles[0].text, "Done");
    }

    /// Item 22: bounds are measured in display columns, not UTF-8 bytes.
    /// `[ 确定 ]` renders as 8 columns (brackets+spaces+4), not 10.
    #[test]
    fn test_button_bounds_are_display_columns() {
        let screen = make_screen(vec!["[ 确定 ]".to_string()], 20);
        let controls = detect_controls(&screen, &[]);
        let btn = controls
            .iter()
            .find(|c| c.kind == ControlKind::Button)
            .expect("button");
        assert_eq!(btn.label, "确定");
        assert_eq!(btn.bounds.x, 0);
        assert_eq!(
            btn.bounds.width, 2,
            "label width in columns, was bytes (=6)"
        );
    }

    /// Item 22: a wide-glyph prefix must not shift the x of later content —
    /// viewport_text rows are column-aligned by CellString (each 界 gets a
    /// filler space at its continuation column), so char index stays equal
    /// to screen column. The fixture row is built by the same builder
    /// `from_vt` uses.
    #[test]
    fn test_column_alignment_survives_wide_prefix() {
        use crate::screen::CellString;
        // 界 at cols 0 and 2 (continuations at 1 and 3), button at col 5.
        let row = CellString::from_cells(vec![(0, "界"), (2, "界"), (5, "[ OK ]")], 12)
            .as_str()
            .to_string();
        let screen = make_screen(vec![row], 20);
        let controls = detect_controls(&screen, &[]);
        let btn = controls
            .iter()
            .find(|c| c.kind == ControlKind::Button)
            .expect("button");
        assert_eq!(btn.label, "OK");
        assert_eq!(
            btn.bounds.x, 5,
            "x is the screen column after two double-width glyphs"
        );
    }

    #[test]
    fn test_extract_toggle_radio() {
        let toggles = extract_toggle("( ) Choice");
        assert_eq!(toggles.len(), 1);
        assert_eq!(toggles[0].kind, ControlKind::Radio);
        assert_eq!(toggles[0].text, "Choice");
    }

    /// Wave-3 item 18: checked state comes from the glyph, not the kind.
    /// `[ ] Foo` and `[x] Foo` are both checkboxes; only the latter is on.
    #[test]
    fn test_toggle_state_from_glyph() {
        let unchecked = extract_toggle("[ ] Foo");
        let checked = extract_toggle("[x] Foo");
        let checked_upper = extract_toggle("[X] Foo");
        assert!(!unchecked[0].checked, "[ ] must be unchecked");
        assert!(checked[0].checked, "[x] must be checked");
        assert!(checked_upper[0].checked, "[X] must be checked");

        let radio_off = extract_toggle("( ) Mode");
        let radio_on = extract_toggle("(*) Mode");
        assert!(!radio_off[0].checked);
        assert!(radio_on[0].checked);
    }

    /// The full-control surface: detect_controls reports the derived state.
    #[test]
    fn test_detect_controls_reports_checked_state() {
        let screen = make_screen(
            vec![
                "[ ] Offline".to_string(),
                "[x] Online".to_string(),
                "(*) Fast".to_string(),
            ],
            40,
        );
        let controls = detect_controls(&screen, &[]);
        let offline = controls
            .iter()
            .find(|c| c.label == "Offline")
            .expect("offline");
        let online = controls
            .iter()
            .find(|c| c.label == "Online")
            .expect("online");
        let fast = controls.iter().find(|c| c.label == "Fast").expect("fast");
        assert!(!offline.checked);
        assert!(online.checked);
        assert!(fast.selected, "(*) is the selected radio option");
        // `checked` reflects the glyph state for any toggle — including the
        // `(*)` radio glyph — so both flags read true for a selected radio.
        assert!(fast.checked);
    }

    /// Wave-3 item 20: a button on one row must not suppress menu recognition
    /// on other rows. Under the old screen-global flag, `File Edit View Help`
    /// lost its menu items the moment any other row carried a `[ Button ]`.
    #[test]
    fn test_menu_row_survives_button_elsewhere() {
        let screen = make_screen(
            vec![
                "File  Edit  View  Help".to_string(),
                "Header: value".to_string(),
                "[ Save ]".to_string(),
            ],
            40,
        );
        let controls = detect_controls(&screen, &[]);
        let menus: Vec<_> = controls
            .iter()
            .filter(|c| c.kind == ControlKind::MenuItem)
            .collect();
        assert!(
            menus.iter().any(|m| m.label.contains("File")),
            "menu bar items must be recognized even though another row has a button/field; got {:?}",
            controls.iter().map(|c| (&c.kind, &c.label)).collect::<Vec<_>>()
        );
    }

    /// Wave-3 item 21: a control inside nested regions binds to the
    /// innermost (smallest) container, not the first/largest hit.
    #[test]
    fn test_control_binds_to_smallest_containing_region() {
        let screen = make_screen(vec!["".to_string(), "  [ Save ]".to_string()], 40);
        let outer = crate::semantic::Region {
            id: "region-outer".into(),
            kind: crate::semantic::RegionKind::Panel,
            title: None,
            bounds: crate::semantic::regions::Bounds {
                x: 0,
                y: 0,
                width: 40,
                height: 10,
            },
            parent_id: None,
            child_ids: vec![],
            clipping_state: crate::semantic::ClippingState::None,
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
        };
        let inner = crate::semantic::Region {
            id: "region-dialog".into(),
            kind: crate::semantic::RegionKind::Dialog,
            title: None,
            bounds: crate::semantic::regions::Bounds {
                x: 2,
                y: 0,
                width: 20,
                height: 4,
            },
            parent_id: Some("region-outer".into()),
            child_ids: vec![],
            clipping_state: crate::semantic::ClippingState::None,
            confidence: crate::semantic::Confidence::inferred(0.9, &["test"]),
        };
        let controls = detect_controls(&screen, &[outer, inner]);
        let save = controls.iter().find(|c| c.label == "Save").expect("save");
        assert_eq!(
            save.region_id.as_deref(),
            Some("region-dialog"),
            "Save lives in the dialog, not the outer panel"
        );
    }

    #[test]
    fn test_is_toggle_text() {
        assert!(is_toggle_text("x"));
        assert!(is_toggle_text("X"));
        assert!(is_toggle_text("*"));
        assert!(is_toggle_text(" "));
        assert!(!is_toggle_text("Save"));
    }

    #[test]
    fn test_mark_consumed_spans() {
        let regions = Vec::new();
        let consumed = mark_consumed_spans("│ Host: localhost │", &regions, 0);
        // Leading "│ " and trailing " │" should be consumed
        assert!(!consumed.is_empty());
    }

    /// Wave-3 item 16: IDs are geometry-free — same kind+label+region yields
    /// the same ID regardless of coordinates.
    #[test]
    fn test_stable_id_deterministic() {
        let id1 = stable_id(&ControlKind::Button, "Save", Some("dialog/settings"));
        let id2 = stable_id(&ControlKind::Button, "Save", Some("dialog/settings"));
        assert_eq!(id1, id2);
        assert_eq!(id1, "dialog/settings/button/save");
    }

    /// The old scheme made position part of the ID; the new scheme must NOT.
    #[test]
    fn test_stable_id_ignores_position() {
        // Position is no longer an input at all — two buttons with the same
        // label collide at ID level and are separated by the `#{n}` second
        // pass, not by geometry.
        let id1 = stable_id(&ControlKind::Button, "Save", None);
        let id2 = stable_id(&ControlKind::Button, "Save", None);
        assert_eq!(id1, id2, "geometry must not leak into IDs");
        assert_eq!(id1, "button/save");
    }

    #[test]
    fn test_stable_id_sanitizes() {
        let id = stable_id(&ControlKind::Field, "Host Name!", None);
        assert_eq!(id, "field/host-name");
    }

    /// Duplicate labels on one screen get `#{n}` disambiguators, not
    /// coordinate suffixes.
    #[test]
    fn test_duplicate_labels_disambiguated() {
        let screen = make_screen(vec!["[ Save ]".to_string(), "[ Save ]".to_string()], 40);
        let controls = detect_controls(&screen, &[]);
        let ids: Vec<&str> = controls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "duplicates must be distinct");
        assert!(ids[0].starts_with("button/save"), "ids: {ids:?}");
        assert!(ids[1].starts_with("button/save#2"), "ids: {ids:?}");
    }

    /// The point of the whole change: moving a control (resize/reflow) keeps
    /// its ID stable.
    #[test]
    fn test_control_id_survives_resize() {
        // Same label, different coordinates — the scenario resize produces.
        let at_a = make_screen(
            vec!["".to_string(), "".to_string(), "      [ Save ]".to_string()],
            60,
        );
        let at_b = make_screen(vec!["[ Save ]".to_string()], 40);
        let a = detect_controls(&at_a, &[]);
        let b = detect_controls(&at_b, &[]);
        let id_a = a.iter().find(|c| c.label == "Save").map(|c| c.id.clone());
        let id_b = b.iter().find(|c| c.label == "Save").map(|c| c.id.clone());
        assert_eq!(id_a, id_b, "resize must not change the control's ID");
        assert_eq!(id_a.as_deref(), Some("button/save"));
    }

    /// Helper: build a ScreenState from rows.
    fn make_screen(rows: Vec<String>, cols: u16) -> ScreenState {
        ScreenState {
            cols,
            rows: rows.len() as u16,
            cursor: crate::screen::CursorState {
                x: 0,
                y: 0,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: rows,
            scrollback: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: crate::screen::ProcessState {
                running: false,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        }
    }

    /// Helper: build a Region with known bounds.
    fn make_region(id: &str, x: u16, y: u16, w: u16, h: u16) -> Region {
        use crate::semantic::regions::Bounds;
        Region {
            id: id.to_string(),
            kind: crate::semantic::regions::RegionKind::Unknown,
            title: None,
            bounds: Bounds {
                x,
                y,
                width: w,
                height: h,
            },
            confidence: Confidence::inferred(0.9, &["test-region"]),
            parent_id: None,
            child_ids: Vec::new(),
            clipping_state: crate::semantic::regions::ClippingState::None,
        }
    }

    // ---- New control kind tests ----

    #[test]
    fn test_detect_spinner_line() {
        let screen = make_screen(vec!["Loading |".to_string()], 40);
        let controls = detect_controls(&screen, &[]);
        let kinds: Vec<_> = controls.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&ControlKind::Spinner));
    }

    #[test]
    fn test_spinner_not_duplicated_as_status() {
        // "Loading |" should emit a Spinner, not a Status
        let screen = make_screen(vec!["Loading |".to_string()], 40);
        let controls = detect_controls(&screen, &[]);
        let kinds: Vec<_> = controls.iter().map(|c| &c.kind).collect();
        assert!(!kinds.contains(&&ControlKind::Status));
        assert!(kinds.contains(&&ControlKind::Spinner));
    }

    #[test]
    fn test_tab_selected_flag() {
        // Test tab detection directly (through the recognizer).
        // Use a line where tabs won't be consumed by button/field extractors.
        let screen = make_screen(vec!["File │ Edit │ View".to_string()], 40);
        let controls = detect_controls(&screen, &[]);
        let tabs: Vec<_> = controls
            .iter()
            .filter(|c| c.kind == ControlKind::Tab)
            .collect();
        // All should be unselected since no brackets.
        for t in &tabs {
            assert!(!t.selected, "tab {:?} should not be selected", t.label);
        }
    }

    #[test]
    fn test_tab_selected_flag_with_brackets() {
        // Directly test the recognizer's bracket detection.
        let cands = recognizers::detect_tabs("[File] │ Edit │ View", 0);
        assert!(cands.len() >= 2);
        assert!(cands.iter().any(|c| c.selected && c.label == "File"));
        assert!(!cands.iter().any(|c| c.selected && c.label == "Edit"));
    }

    #[test]
    fn test_focusable_by_kind() {
        let screen = make_screen(
            vec![
                "[ Save ]".to_string(),        // Button -> focusable
                "Host: localhost".to_string(), // Field -> focusable
                "Status: OK".to_string(),      // Status -> not focusable
                "[====>    ]".to_string(),     // Progress -> not focusable
            ],
            50,
        );
        let controls = detect_controls(&screen, &[]);
        for ctrl in &controls {
            match ctrl.kind {
                ControlKind::Button | ControlKind::Field => {
                    assert!(ctrl.focusable, "{:?} should be focusable", ctrl.kind);
                }
                ControlKind::Status | ControlKind::Progress | ControlKind::Spinner => {
                    assert!(!ctrl.focusable, "{:?} should not be focusable", ctrl.kind);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_region_id_assignment() {
        let region = make_region("reg-0", 0, 0, 80, 24);
        let screen = make_screen(
            vec![
                "│ Host: localhost        │".to_string(), // inside region
                "[ Save ]".to_string(),                   // inside region
            ],
            40,
        );
        let controls = detect_controls(&screen, &[region]);
        // Controls should have region_id set
        for ctrl in &controls {
            assert_eq!(ctrl.region_id.as_deref(), Some("reg-0"));
        }
    }

    #[test]
    fn test_no_button_for_checkbox_line() {
        // "[x] Option" should produce Checkbox, not also a Button
        let screen = make_screen(vec!["[x] Option".to_string()], 40);
        let controls = detect_controls(&screen, &[]);
        let kinds: Vec<_> = controls.iter().map(|c| &c.kind).collect();
        assert!(kinds.contains(&&ControlKind::Checkbox));
        // There should be exactly 1 control (checkbox only, not button too)
        assert_eq!(controls.len(), 1);
    }

    #[test]
    fn test_overlaps_consumed() {
        let consumed = vec![(0, 5), (10, 20)];
        assert!(!overlaps_consumed(&consumed, 6, 9)); // gap between spans
        assert!(overlaps_consumed(&consumed, 3, 7)); // overlaps first
        assert!(overlaps_consumed(&consumed, 8, 11)); // overlaps second
        assert!(overlaps_consumed(&consumed, 0, 1)); // inside first
        assert!(overlaps_consumed(&consumed, 15, 16)); // inside second
    }
}
