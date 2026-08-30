//! Volatile-content normalization for the structure hash (spec section 12).
//!
//! Fixes item 19: normalize per-row (full line text) rather than per-cell.
//! A single cell usually contains one grapheme, so patterns like
//! `\b\d\d:\d\d:\d\b` (clock) or `\bCPU \d+%` (progress) can never match
//! within a single cell. Normalizing full rendered rows first lets these
//! patterns collapse correctly.

use regex::Regex;

/// Normalize a single cell's text (mostly a passthrough now — actual
/// normalization happens at the row level via `normalize_row`).
pub fn normalize_text(s: &str) -> String {
    s.to_string()
}

/// Normalize a full rendered row. This is the proper place to collapse
/// volatile tokens like timestamps, percentages, and counters so that
/// structurally-identical screens hash equal.
///
/// Steps:
///   1. Collapse whitespace runs (helps when cells contain individual
///      spaces between tokens).
///   2. Replace percentage tokens (e.g. "37%", "100.0%").
///   3. Replace clock timestamps (e.g. "12:42:03").
///   4. Replace isolated bare integers (preserving one "N" marker).
pub fn normalize_row(s: &str) -> String {
    thread_local! {
        static WS: Regex = Regex::new(r"\s+").unwrap();
        static PCT: Regex = Regex::new(r"\b\d+(\.\d+)?%").unwrap();
        static CLOCK: Regex = Regex::new(r"\b(\d{1,2}):(\d{2})(:(\d{2}))?\b").unwrap();
        // Standalone integers (word boundaries ensure we match whole numbers).
        static NUM: Regex = Regex::new(r"\b\d+\b").unwrap();
    }

    let s = WS.with(|re| re.replace_all(s, " "));
    let s = PCT.with(|re| re.replace_all(&s, "N%").into_owned());
    let s = CLOCK.with(|re| re.replace_all(&s, "HH:MM:SS").into_owned());
    let s = NUM.with(|re| re.replace_all(&s, "N").into_owned());
    s.trim().to_string()
}

/// Structure hash cell contribution. Instead of normalizing the cell text
/// alone, we normalize the whole row context and then contribute only this
/// cell's portion (if it falls within a matched volatile span, the whole
/// span gets collapsed).
pub fn normalize_cell_in_row(row: &str, cell_start: usize, cell_end: usize) -> String {
    // The simple approach: normalize the full row, then take the substring
    // corresponding to this cell's span in the normalized output. This is
    // only approximate — proper span tracking would need the regex match
    // offsets. For our use case the approximation suffices: the structure
    // hash doesn't need to be human-readable, only stable.
    let normalized = normalize_row(row);
    if cell_start >= normalized.len() {
        return String::new();
    }
    let end = cell_end.min(normalized.len());
    normalized[cell_start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_row_percentage() {
        assert_eq!(normalize_row("CPU 37%"), "CPU N%");
        assert_eq!(normalize_row("Progress: 100.0%"), "Progress: N%");
    }

    #[test]
    fn test_normalize_row_clock() {
        assert_eq!(normalize_row("12:42:03"), "HH:MM:SS");
        assert_eq!(normalize_row("Time is 9:30"), "Time is HH:MM:SS");
    }

    #[test]
    fn test_normalize_row_bare_number() {
        assert_eq!(normalize_row("Count: 42"), "Count: N");
        assert_eq!(normalize_row("  7 "), "N");
    }

    #[test]
    fn test_normalize_row_preserves_context() {
        // "Port: 8080" should keep the label but collapse the number
        assert_eq!(normalize_row("Port: 8080"), "Port: N");
    }

    #[test]
    fn test_normalize_row_no_volatile() {
        assert_eq!(normalize_row("Hello World"), "Hello World");
        assert_eq!(normalize_row("Save"), "Save");
    }

    #[test]
    fn test_normalize_row_multiple_patterns() {
        assert_eq!(
            normalize_row("CPU 45% at 12:42:03, count 7"),
            "CPU N% at HH:MM:SS, count N"
        );
    }
}
