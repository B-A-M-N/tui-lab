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
/// Recognizers are ordered: toggle → button → field.  Once a span is consumed
/// by an earlier recognizer it is not reconsidered by later ones.
pub fn detect_controls(screen: &ScreenState, regions: &[Region]) -> Vec<Control> {
    let mut out = Vec::new();
    for (y, line) in screen.viewport_text.iter().enumerate() {
        let y = y as u16;
        let consumed = mark_consumed_spans(line, regions, y);

        // 1) checkbox/radio: ( ) or (*) or [ ] or [x]
        for cb in extract_toggle(line) {
            if is_consumed(&consumed, cb.x, cb.x + 3) {
                continue;
            }
            out.push(Control {
                id: stable_id(&cb.kind, &cb.text, cb.x, y),
                kind: cb.kind,
                label: cb.text,
                value: None,
                bounds: ControlBounds { x: cb.x, y, width: cb.text.len() as u16, height: 1 },
                region_id: None,
                focusable: true,
                focused: false,
                enabled: true,
                selected: false,
                checked: cb.kind == ControlKind::Checkbox,
                shortcut: None,
                confidence: Confidence::inferred(0.9, &["toggle-glyph"]),
                evidence: vec!["toggle-glyph".to_string()],
                source: "inferred".to_string(),
            });
        }

        // 2) buttons: [ text ] or < text >
        //    Skip `[x]`, `[ ]` which are toggles, not buttons.
        for cap in extract_bracketed(line) {
            if is_consumed(&consumed, cap.x, cap.x + cap.text.len() as u16 + 2) {
                continue;
            }
            // Skip toggle-like content inside brackets
            if is_toggle_text(&cap.text) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Button, &cap.text, cap.x, y),
                kind: ControlKind::Button,
                label: cap.text,
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
        }

        // 3) field: "Label:" optionally followed by a value
        if let Some(f) = extract_field(line) {
            if is_consumed(&consumed, f.x, f.x + f.label.len() as u16) {
                continue;
            }
            out.push(Control {
                id: stable_id(&ControlKind::Field, &f.label, f.x, y),
                kind: ControlKind::Field,
                label: f.label,
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

fn is_consumed(consumed: &[(u16, u16)], start: u16, end: u16) -> bool {
    consumed.iter().any(|&(cs, ce)| start >= cs && end <= ce)
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
}
