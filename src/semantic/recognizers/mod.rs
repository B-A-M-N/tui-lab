//! Recognizer modules for additional control kinds (spec item 25).
//!
//! Each recognizer returns candidates with confidence/evidence.
//! A merge/resolution pass in `controls.rs` eliminates overlaps.

use crate::screen::display_width;
use crate::semantic::confidence::Confidence;
use crate::semantic::controls::ControlKind;

/// A candidate control detected by a recognizer.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: ControlKind,
    pub label: String,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub confidence: Confidence,
    pub evidence: Vec<&'static str>,
    /// For fields and status: the value after the label.
    pub value: Option<String>,
    /// Whether the candidate is focusable (interactive).
    pub focusable: bool,
    /// Selected state (for tabs, list items, menu items).
    pub selected: bool,
    /// Keyboard shortcut hint (e.g. "&File" -> "F").
    pub shortcut: Option<String>,
}

/// Detect tab-like controls: "Tab 1 | Tab 2 | Tab 3" or "[Tab 1] [Tab 2]"
/// Accepts >= 2 segments. Selects one if a bracketed marker like "[Tab]" is
/// detected. If no selected marker is present, emit all with selected=false.
pub fn detect_tabs(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();

    // Pattern: text separated by │ or |
    let separators = ['│', '|', '┃', '║'];
    let mut segments: Vec<(u16, u16, String)> = Vec::new();
    let mut seg_start = 0u16;

    for (i, &c) in chars.iter().enumerate() {
        if separators.contains(&c) {
            if i > seg_start as usize {
                let text: String = chars[seg_start as usize..i].iter().collect();
                segments.push((seg_start, i as u16, text));
            }
            seg_start = (i + 1) as u16;
        }
    }
    if seg_start < chars.len() as u16 {
        let text: String = chars[seg_start as usize..].iter().collect();
        segments.push((seg_start, chars.len() as u16, text));
    }

    if segments.len() >= 2 && segments.len() <= 8 {
        for (start, end, text) in &segments {
            let trimmed = text.trim();
            if !trimmed.is_empty() && trimmed.chars().count() <= 20 {
                // Check if this segment is bracketed like "[Tab]" which
                // conventionally marks the selected tab.
                let selected = trimmed.starts_with('[') && trimmed.ends_with(']');
                let label = if selected {
                    // Strip ASCII brackets and re-trim inner content (char
                    // boundary-safe: brackets are 1 byte, inner may not be).
                    let inner = &trimmed['['.len_utf8()..trimmed.len() - ']'.len_utf8()];
                    inner.trim().to_string()
                } else {
                    trimmed.to_string()
                };
                out.push(Candidate {
                    kind: ControlKind::Tab,
                    label,
                    x: *start,
                    y,
                    width: end - start, // segment bounds are column arithmetic
                    confidence: Confidence::inferred(0.7, &["tab-segment"]),
                    evidence: vec!["separator-delimited"],
                    value: None,
                    focusable: true,
                    selected,
                    shortcut: None,
                });
            }
        }
    }

    out
}

/// Detect list items: " • item" or " - item" or " 1. item"
pub fn detect_list_items(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    // Bullet points
    let bullet_prefix = trimmed
        .strip_prefix("• ")
        .or_else(|| trimmed.strip_prefix("- "))
        .or_else(|| trimmed.strip_prefix("* "));
    if let Some(rest) = bullet_prefix {
        let label = rest.trim().to_string();
        if !label.is_empty() {
            // Bullet glyph "•" is one column; "-"/"*" are ASCII — the width
            // is indent + display width of the trimmed remainder.
            let width = indent as u16 + display_width(trimmed);
            out.push(Candidate {
                kind: ControlKind::List,
                label,
                x: indent as u16,
                y,
                width,
                confidence: Confidence::inferred(0.8, &["bullet-list"]),
                evidence: vec!["bullet-glyph"],
                value: None,
                focusable: true,
                selected: false,
                shortcut: None,
            });
        }
    }

    // Numbered list
    if trimmed.len() > 3 {
        let first_char = trimmed.chars().next().unwrap();
        if first_char.is_ascii_digit() {
            if let Some(dot_pos) = trimmed.find(". ") {
                if dot_pos <= 3 && trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
                    let label = trimmed[dot_pos + 2..].trim().to_string();
                    if !label.is_empty() {
                        // Numbered prefix "N. " is ASCII; width follows the
                        // display width of the full item text.
                        let width = indent as u16 + display_width(trimmed);
                        out.push(Candidate {
                            kind: ControlKind::List,
                            label,
                            x: indent as u16,
                            y,
                            width,
                            confidence: Confidence::inferred(0.8, &["numbered-list"]),
                            evidence: vec!["number-prefix"],
                            value: None,
                            focusable: true,
                            selected: false,
                            shortcut: None,
                        });
                    }
                }
            }
        }
    }

    out
}

/// Detect menu items: "File  Edit  View  Help"
///
/// Conservative: only emit when the line contains no colon-field separator
/// and no bracketed button. Extract shortcut from "&X" or "(X)text" patterns.
pub fn detect_menu_items(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim();

    // Be conservative: no colon-field and no bracketed button
    if trimmed.contains(':') || trimmed.contains('[') || trimmed.contains('<') {
        return out;
    }

    // Split by 2+ whitespace to detect multi-word items with gaps
    // Use split_ascii_whitespace to find individual words
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 2 && words.len() <= 10 {
        let all_short = words.iter().all(|w| w.len() <= 10);
        let has_menu_words = words.iter().any(|w| {
            matches!(
                w.to_lowercase().as_str(),
                "file" | "edit" | "view" | "help" | "options" | "tools" | "window"
            )
        });

        if all_short && has_menu_words {
            let mut pos = 0usize; // byte offset into trimmed
            for word in &words {
                if let Some(idx) = trimmed[pos..].find(word) {
                    // x is a screen column: measure the prefix's display
                    // width instead of using the byte offset (item 22).
                    let x = display_width(&trimmed[..pos + idx]);
                    // Extract shortcut: "&X" or "(X)text"
                    let shortcut = if word.starts_with('&') && word.len() > 1 {
                        // Shortcut char is after the ampersand
                        Some(
                            word[1..]
                                .chars()
                                .next()
                                .unwrap_or('\0')
                                .to_uppercase()
                                .to_string(),
                        )
                    } else if let Some(inner) = word.strip_prefix('(') {
                        // e.g. "(F)ile" — extract first parenthesized letter
                        inner
                            .chars()
                            .find(|c| *c != ')')
                            .map(|c| c.to_uppercase().to_string())
                    } else {
                        None
                    };
                    out.push(Candidate {
                        kind: ControlKind::MenuItem,
                        label: word.to_string(),
                        x,
                        y,
                        // Display width, not UTF-8 bytes: a wide label must
                        // claim its rendered columns (item 22).
                        width: display_width(word),
                        confidence: Confidence::inferred(0.75, &["menu-bar"]),
                        evidence: vec!["menu-context"],
                        value: None,
                        focusable: true,
                        selected: false,
                        shortcut,
                    });
                    pos = pos + idx + word.len(); // byte offset, not column
                }
            }
        }
    }

    out
}

/// Detect status values: "Status: Connected" or "State: Running"
///
/// Confidence lowered to 0.6 so it does not compete with Button/Field.
/// The caller (detect_controls) must skip this pass on lines that already
/// produced a Button or Field control.
pub fn detect_status(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let status_keywords = ["status:", "state:", "connection:", "mode:"];

    let lower = line.to_lowercase();
    for keyword in &status_keywords {
        if let Some(idx) = lower.find(keyword) {
            let after = &line[idx + keyword.len()..].trim();
            if !after.is_empty() {
                // x in columns: the lowercase prefix has the same display
                // width as the original text.
                let x = display_width(&line[..idx + keyword.len()]);
                out.push(Candidate {
                    kind: ControlKind::Status,
                    label: after.to_string(),
                    x,
                    y,
                    width: display_width(after),
                    confidence: Confidence::inferred(0.6, &["status-label"]),
                    evidence: vec!["status-keyword"],
                    value: Some(after.to_string()),
                    focusable: false,
                    selected: false,
                    shortcut: None,
                });
            }
            break;
        }
    }

    out
}

/// Detect progress indicators: "[====>    ]" or "50%"
///
/// Requires an actual percent sign (0-100) or a filled/empty bar pair
/// (█/░ or #/- or =/space). Rejects lines that merely contain digits.
pub fn detect_progress(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim();

    // 1) Bracketed progress bar: [====>    ]
    if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 4 {
        // Byte-safe inner slice: first/last chars are ASCII brackets, so
        // char_indices on the remainder stays inside the ASCII envelope.
        let inner = &trimmed[1..trimmed.len() - ']'.len_utf8()];
        let has_progress_chars = inner
            .chars()
            .any(|c| c == '=' || c == '#' || c == '█' || c == '▓');
        let has_incomplete = inner.chars().any(|c| c == ' ' || c == '.' || c == '░');

        if has_progress_chars && has_incomplete {
            let x = display_width(line) - display_width(trimmed);
            out.push(Candidate {
                kind: ControlKind::Progress,
                label: "progress".to_string(),
                x,
                y,
                width: display_width(trimmed),
                confidence: Confidence::inferred(0.9, &["progress-bar"]),
                evidence: vec!["bar-glyphs"],
                value: None,
                focusable: false,
                selected: false,
                shortcut: None,
            });
            return out; // Don't also match the % path on the same line
        }
    }

    // 2) Percent: "50%"
    //    Require actual percent sign; ensure the number is 0-100
    if let Some(pct_idx) = trimmed.find('%') {
        let before_pct = &trimmed[..pct_idx];
        // Find the last contiguous digit sequence before %
        let trimmed_end = before_pct.trim_end();
        let digit_start = trimmed_end
            .rfind(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        let num_str = &trimmed_end[digit_start..];

        if let Ok(pct) = num_str.parse::<u8>() {
            if pct <= 100 && !num_str.is_empty() {
                // Column arithmetic: digits + '%' are ASCII, so the number's
                // width is its char count; its column is the display width
                // of everything before it.
                let num_start = display_width(trimmed_end) - num_str.chars().count() as u16;
                out.push(Candidate {
                    kind: ControlKind::Progress,
                    label: format!("{}%", pct),
                    x: display_width(line) - display_width(trimmed) + num_start,
                    y,
                    width: num_str.chars().count() as u16 + 1,
                    confidence: Confidence::inferred(0.85, &["percentage"]),
                    evidence: vec!["percent-sign"],
                    value: None,
                    focusable: false,
                    selected: false,
                    shortcut: None,
                });
            }
        }
    }

    out
}

/// Detect spinners: "|", "/", "\", Unicode spinner chars like ◐◑◒◓◴◵◶◷
///
/// Only matches when the spinner char appears in a "Loading |" context.
/// Does NOT match "-" or "|" when part of a progress bar (inside [])
/// or when part of tab separators.
pub fn detect_spinner(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim();

    // Unicode spinner chars are distinctive enough
    let spinner_unique: [char; 9] = ['◐', '◑', '◒', '◓', '◔', '◴', '◵', '◶', '◷'];
    // Simple spinner chars that need context
    let spinner_simple: [char; 3] = ['|', '/', '\\'];

    // Check for Unicode spinner chars first
    let trim_cols = display_width(line) - display_width(trimmed);
    for (i, c) in trimmed.char_indices() {
        if spinner_unique.contains(&c) {
            // Column, not byte offset: measure the prefix width (item 22).
            let x = trim_cols + display_width(&trimmed[..i]);
            out.push(Candidate {
                kind: ControlKind::Spinner,
                label: "loading".to_string(),
                x,
                y,
                width: 1,
                confidence: Confidence::inferred(0.85, &["spinner-glyph"]),
                evidence: vec!["unicode-spinner"],
                value: None,
                focusable: false,
                selected: false,
                shortcut: None,
            });
            return out;
        }
    }

    // For simple spinner chars, require standalone context
    for (i, c) in trimmed.char_indices() {
        if spinner_simple.contains(&c) {
            // Skip if inside a bracketed progress bar
            let in_bracket = trimmed.starts_with('[')
                && trimmed.ends_with(']')
                && i > 0
                && i < trimmed.len() - 1;
            if in_bracket {
                continue;
            }
            let prev_char = trimmed.chars().nth(i.saturating_sub(1));
            let next_char = trimmed.chars().nth(i + 1);
            let prev_ok = prev_char
                .map(|c| c == ' ' || c.is_alphanumeric())
                .unwrap_or(true);
            let next_ok = next_char
                .map(|c| c == ' ' || c.is_alphanumeric())
                .unwrap_or(true);
            if prev_ok && next_ok {
                let x = trim_cols + display_width(&trimmed[..i]);
                out.push(Candidate {
                    kind: ControlKind::Spinner,
                    label: "loading".to_string(),
                    x,
                    y,
                    width: 1,
                    confidence: Confidence::inferred(0.7, &["spinner-char"]),
                    evidence: vec!["spinning-glyph"],
                    value: None,
                    focusable: false,
                    selected: false,
                    shortcut: None,
                });
                return out;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_tabs() {
        let candidates = detect_tabs(" File │ Edit │ View │ Help ", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::Tab);
        // Default: none selected (no brackets)
        assert!(!candidates[0].selected);
    }

    #[test]
    fn test_detect_tabs_selected() {
        let candidates = detect_tabs("[ File ] │ Edit │ View ", 0);
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0].selected);
        assert_eq!(candidates[0].label, "File");
        assert!(!candidates[1].selected);
        assert!(!candidates[2].selected);
    }

    #[test]
    fn test_detect_list_items_bullet() {
        let candidates = detect_list_items(" • First item", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::List);
        assert_eq!(candidates[0].label, "First item");
        assert!(candidates[0].focusable);
    }

    #[test]
    fn test_detect_list_items_numbered() {
        let candidates = detect_list_items("1. First item", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::List);
        assert_eq!(candidates[0].label, "First item");
        assert!(candidates[0].focusable);
    }

    #[test]
    fn test_detect_menu_items() {
        let candidates = detect_menu_items("File  Edit  View  Help", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::MenuItem);
        assert!(candidates[0].focusable);
    }

    #[test]
    fn test_detect_menu_items_no_field_line() {
        // Lines with colon-field should be skipped
        let candidates = detect_menu_items("File: option", 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_detect_menu_items_no_button_line() {
        // Lines with brackets should be skipped
        let candidates = detect_menu_items("[File] [Edit]", 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_detect_menu_items_with_shortcut() {
        let candidates = detect_menu_items("&File  Edit  &Help", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].shortcut, Some("F".to_string()));
    }

    #[test]
    fn test_detect_status() {
        let candidates = detect_status("Status: Connected", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::Status);
        assert_eq!(candidates[0].label, "Connected");
        assert_eq!(candidates[0].confidence.score, 0.6);
        assert!(!candidates[0].focusable);
    }

    #[test]
    fn test_detect_progress_bar() {
        let candidates = detect_progress("[====>    ]", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::Progress);
        assert!(!candidates[0].focusable);
    }

    #[test]
    fn test_detect_progress_percent() {
        let candidates = detect_progress("Progress: 75%", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::Progress);
    }

    #[test]
    fn test_detect_progress_rejects_digits_only() {
        // A line with digits but no % sign or bar should not match
        let candidates = detect_progress("Total: 42", 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_detect_spinner() {
        let candidates = detect_spinner("Loading |", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::Spinner);
        assert!(!candidates[0].focusable);
    }

    #[test]
    fn test_detect_spinner_unicode() {
        let line = "Loading \u{25d4}";
        let trimmed = line.trim();
        eprintln!(
            "line={:?} trimmed={:?} char_count={} char_lens={:?}",
            line,
            trimmed,
            trimmed.chars().count(),
            trimmed.chars().map(|c| c.len_utf8()).collect::<Vec<_>>()
        );
        let candidates = detect_spinner(line, 0);
        eprintln!(
            "candidates={:?}",
            candidates.iter().map(|c| &c.kind).collect::<Vec<_>>()
        );
        assert!(!candidates.is_empty(), "expected spinner for {:?}", line);
        assert_eq!(candidates[0].kind, ControlKind::Spinner);
    }

    #[test]
    fn test_detect_spinner_no_bar() {
        // Spinner char inside a progress bar should not match
        let candidates = detect_spinner("[|----    ]", 0);
        assert!(candidates.is_empty());
    }
}
