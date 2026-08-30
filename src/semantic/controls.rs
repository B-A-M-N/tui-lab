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
use crate::semantic::regions::Region;
use crate::semantic::recognizers;

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

fn default_true() -> bool { true }
fn default_source() -> String { "inferred".to_string() }

/// Check if point (px, py) is within bounds [x, x+width) x [y, y+height).
fn region_contains_point(bounds: &crate::semantic::regions::Bounds, px: u16, py: u16) -> bool {
    px >= bounds.x
        && py >= bounds.y
        && px < bounds.x + bounds.width
        && py < bounds.y + bounds.height
}

/// Generate a stable control ID from kind, label, and geometry.
fn stable_id(kind: &ControlKind, label: &str, x: u16, y: u16) -> String {
    let kind_str = format!("{:?}", kind).to_lowercase();
    // Sanitize label: lowercase, alphanumeric + hyphens, max 20 chars
    let sanitized: String = label
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(20)
        .collect();
    let sanitized = if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    };
    format!("{}:{}:{},{}", kind_str, sanitized, x, y)
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
    let any_line_has_button_or_field = screen
        .viewport_text
        .iter()
        .any(|line| extract_bracketed(line).iter().any(|cap| !is_toggle_text(&cap.text))
            || extract_field(line).is_some());

    for (y, line) in screen.viewport_text.iter().enumerate() {
        let y = y as u16;
        let mut consumed: Vec<(u16, u16)> = mark_consumed_spans(line, regions, y);

        // 1) checkbox/radio: ( ) or (*) or [ ] or [x]
        for cb in extract_toggle(line) {
            if overlaps_consumed(&consumed, cb.x, cb.x + 3) {
                continue;
            }
            out.push(Control {
                id: stable_id(&cb.kind, &cb.text, cb.x, y),
                checked: cb.kind == ControlKind::Checkbox,
                kind: cb.kind.clone(),
                label: cb.text.clone(),
                value: None,
                bounds: ControlBounds { x: cb.x, y, width: cb.text.len() as u16, height: 1 },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
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
            let span_end = cap.x + cap.text.len() as u16 + 2;
            if overlaps_consumed(&consumed, cap.x, span_end) {
                continue;
            }
            if is_toggle_text(&cap.text) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Button, &cap.text, cap.x, y),
                kind: ControlKind::Button,
                label: cap.text.clone(),
                value: None,
                bounds: ControlBounds { x: cap.x, y, width: cap.text.len() as u16, height: 1 },
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
            if overlaps_consumed(&consumed, f.x, f.x + f.label.len() as u16) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Field, &f.label, f.x, y),
                kind: ControlKind::Field,
                label: f.label.clone(),
                value: f.value,
                bounds: ControlBounds { x: f.x, y, width: f.label.len() as u16, height: 1 },
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
            consumed.push((f.x, f.x + f.label.len() as u16));
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
        if !any_line_has_button_or_field {
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
        if !any_line_has_button_or_field {
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

    // Region linking: assign region_id to each control
    for ctrl in &mut out {
        for region in regions {
            if region_contains_point(&region.bounds, ctrl.bounds.x, ctrl.bounds.y) {
                ctrl.region_id = Some(region.id.clone());
                break;
            }
        }
    }

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
fn candidate_to_control(
    c: &recognizers::Candidate,
    y: u16,
    source: &str,
) -> Control {
    let kind_str = format!("{:?}", c.kind).to_lowercase();
    let sanitized: String = c.label
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
        .take(20)
        .collect();
    let sanitized = if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    };
    let id = format!("{}:{}:{},{}", kind_str, sanitized, c.x, y);

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
}

fn extract_toggle(line: &str) -> Vec<Toggle> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    for i in 0..chars.len() {
        if i + 3 > chars.len() {
            continue;
        }
        let triple: String = chars[i..i + 3].iter().collect();
        let kind = match triple.as_str() {
            "( )" => ControlKind::Radio,
            "(*) " | "(*)" => ControlKind::Radio,
            "[ ]" => ControlKind::Checkbox,
            "[x]" | "[X]" => ControlKind::Checkbox,
            _ => continue,
        };
        // text after the toggle glyph
        let text: String = chars[(i + 3).min(chars.len())..]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        out.push(Toggle {
            kind,
            text,
            x: i as u16,
        });
    }
    out
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

    #[test]
    fn test_extract_toggle_radio() {
        let toggles = extract_toggle("( ) Choice");
        assert_eq!(toggles.len(), 1);
        assert_eq!(toggles[0].kind, ControlKind::Radio);
        assert_eq!(toggles[0].text, "Choice");
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

    #[test]
    fn test_stable_id_deterministic() {
        let id1 = stable_id(&ControlKind::Button, "Save", 10, 5);
        let id2 = stable_id(&ControlKind::Button, "Save", 10, 5);
        assert_eq!(id1, id2);
        assert_eq!(id1, "button:save:10,5");
    }

    #[test]
    fn test_stable_id_different_positions() {
        let id1 = stable_id(&ControlKind::Button, "Save", 10, 5);
        let id2 = stable_id(&ControlKind::Button, "Save", 20, 5);
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_stable_id_sanitizes() {
        let id = stable_id(&ControlKind::Field, "Host Name!", 0, 0);
        assert_eq!(id, "field:hostname:0,0");
    }

    /// Helper: build a ScreenState from rows.
    fn make_screen(rows: Vec<String>, cols: u16) -> ScreenState {
        ScreenState {
            cols,
            rows: rows.len() as u16,
            cursor: crate::screen::CursorState {
                x: 0, y: 0, visible: true,
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
            bounds: Bounds { x, y, width: w, height: h },
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
                "[ Save ]".to_string(),    // Button -> focusable
                "Host: localhost".to_string(), // Field -> focusable
                "Status: OK".to_string(),       // Status -> not focusable
                "[====>    ]".to_string(),  // Progress -> not focusable
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
                "[ Save ]".to_string(),                     // inside region
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
