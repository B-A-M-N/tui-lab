//! Recognizer modules for additional control kinds (spec item 25).
//!
//! Each recognizer returns candidates with confidence/evidence.
//! A merge/resolution pass in `controls.rs` eliminates overlaps.

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
}

/// Detect tab-like controls: "Tab 1 | Tab 2 | Tab 3" or "[Tab 1] [Tab 2]"
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
            if !trimmed.is_empty() && trimmed.len() <= 20 {
                out.push(Candidate {
                    kind: ControlKind::Tab,
                    label: trimmed.to_string(),
                    x: *start,
                    y,
                    width: end - start,
                    confidence: Confidence::inferred(0.7, &["tab-segment"]),
                    evidence: vec!["separator-delimited"],
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
            out.push(Candidate {
                kind: ControlKind::List,
                label,
                x: indent as u16,
                y,
                width: trimmed.len() as u16,
                confidence: Confidence::inferred(0.8, &["bullet-list"]),
                evidence: vec!["bullet-glyph"],
            });
        }
    }

    if trimmed.len() > 3 {
        let first_char = trimmed.chars().next().unwrap();
        if first_char.is_ascii_digit() {
            if let Some(dot_pos) = trimmed.find(". ") {
                if dot_pos <= 3 && trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
                    let label = trimmed[dot_pos + 2..].trim().to_string();
                    if !label.is_empty() {
                        out.push(Candidate {
                            kind: ControlKind::List,
                            label,
                            x: indent as u16,
                            y,
                            width: trimmed.len() as u16,
                            confidence: Confidence::inferred(0.8, &["numbered-list"]),
                            evidence: vec!["number-prefix"],
                        });
                    }
                }
            }
        }
    }

    out
}

/// Detect menu items: "File  Edit  View  Help"
pub fn detect_menu_items(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim();

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
            let mut pos = 0;
            for word in &words {
                if let Some(idx) = trimmed[pos..].find(word) {
                    let x = (pos + idx) as u16;
                    out.push(Candidate {
                        kind: ControlKind::MenuItem,
                        label: word.to_string(),
                        x,
                        y,
                        width: word.len() as u16,
                        confidence: Confidence::inferred(0.75, &["menu-bar"]),
                        evidence: vec!["menu-context"],
                    });
                    pos = x as usize + word.len();
                }
            }
        }
    }

    out
}

/// Detect status values: "Status: Connected" or "State: Running"
pub fn detect_status(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let status_keywords = ["status:", "state:", "connection:", "mode:"];

    let lower = line.to_lowercase();
    for keyword in &status_keywords {
        if let Some(idx) = lower.find(keyword) {
            let after = &line[idx + keyword.len()..].trim();
            if !after.is_empty() {
                out.push(Candidate {
                    kind: ControlKind::Status,
                    label: after.to_string(),
                    x: (idx + keyword.len()) as u16,
                    y,
                    width: after.len() as u16,
                    confidence: Confidence::inferred(0.85, &["status-label"]),
                    evidence: vec!["status-keyword"],
                });
            }
            break;
        }
    }

    out
}

/// Detect progress indicators: "[====>    ]" or "50%"
pub fn detect_progress(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let trimmed = line.trim();

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let has_progress_chars = inner
            .chars()
            .any(|c| c == '=' || c == '#' || c == '█' || c == '▓');
        let has_incomplete = inner.chars().any(|c| c == ' ' || c == '.' || c == '░');

        if has_progress_chars && has_incomplete {
            out.push(Candidate {
                kind: ControlKind::Progress,
                label: "progress".to_string(),
                x: (line.len() - trimmed.len()) as u16,
                y,
                width: trimmed.len() as u16,
                confidence: Confidence::inferred(0.9, &["progress-bar"]),
                evidence: vec!["bar-glyphs"],
            });
        }
    }

    if let Some(pct_idx) = trimmed.find('%') {
        let before_pct = &trimmed[..pct_idx];
        // Find the last contiguous digit sequence before %
        let trimmed_end = before_pct.trim_end();
        let digit_start = trimmed_end.rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1).unwrap_or(0);
        let num_str = &trimmed_end[digit_start..];
        let num_start = before_pct.len() - trimmed_end.len() + digit_start;

        if let Ok(pct) = num_str.parse::<u8>() {
            if pct <= 100 {
                out.push(Candidate {
                    kind: ControlKind::Progress,
                    label: format!("{}%", pct),
                    x: (line.len() - trimmed.len() + num_start) as u16,
                    y,
                    width: (pct.to_string().len() + 1) as u16,
                    confidence: Confidence::inferred(0.85, &["percentage"]),
                    evidence: vec!["percent-sign"],
                });
            }
        }
    }

    out
}

/// Detect spinners: "|", "/", "-", "\", "◐", "◑"
pub fn detect_spinner(line: &str, y: u16) -> Vec<Candidate> {
    let mut out = Vec::new();
    let spinner_chars = ['|', '/', '-', '\\', '◐', '◑', '◒', '◓', '◴', '◵', '◶', '◷'];

    for (i, c) in line.chars().enumerate() {
        if spinner_chars.contains(&c) {
            let prev_char = if i > 0 { line.chars().nth(i - 1) } else { None };
            let next_char = line.chars().nth(i + 1);

            let prev_ok = prev_char.map(|c| c == ' ' || c == '[').unwrap_or(true);
            let next_ok = next_char.map(|c| c == ' ' || c == ']').unwrap_or(true);

            if prev_ok && next_ok {
                out.push(Candidate {
                    kind: ControlKind::Spinner,
                    label: "loading".to_string(),
                    x: i as u16,
                    y,
                    width: 1,
                    confidence: Confidence::inferred(0.7, &["spinner-char"]),
                    evidence: vec!["spinning-glyph"],
                });
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
    }

    #[test]
    fn test_detect_list_items_bullet() {
        let candidates = detect_list_items(" • First item", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::List);
        assert_eq!(candidates[0].label, "First item");
    }

    #[test]
    fn test_detect_list_items_numbered() {
        let candidates = detect_list_items("1. First item", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::List);
        assert_eq!(candidates[0].label, "First item");
    }

    #[test]
    fn test_detect_menu_items() {
        let candidates = detect_menu_items("File  Edit  View  Help", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::MenuItem);
    }

    #[test]
    fn test_detect_status() {
        let candidates = detect_status("Status: Connected", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::Status);
        assert_eq!(candidates[0].label, "Connected");
    }

    #[test]
    fn test_detect_progress_bar() {
        let candidates = detect_progress("[====>    ]", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, ControlKind::Progress);
    }

    #[test]
    fn test_detect_progress_percent() {
        let candidates = detect_progress("Progress: 75%", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::Progress);
    }

    #[test]
    fn test_detect_spinner() {
        let candidates = detect_spinner("Loading |", 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].kind, ControlKind::Spinner);
    }
}
