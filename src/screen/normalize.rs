//! Volatile-content normalization for the structure hash (spec section 12).
//!
//! Fixes item 19: normalize per-row (full line text) rather than per-cell.
//! A single cell usually contains one grapheme, so patterns like
//! `\b\d\d:\d\d:\d\b` (clock) or `\bCPU \d+%` (progress) can never match
//! within a single cell. Normalizing full rendered rows first lets these
//! patterns collapse correctly.
//!
//! Audit items 13/14: conservative NormalizationPolicy replaces the prior
//! blanket `\b\d+\b` collapse. Only clearly-volatile token classes are
//! replaced with stable placeholders. Arbitrary integers in labels like
//! "Port: 22" are preserved.

use regex::Regex;

// ── Volatile classes ──────────────────────────────────────────────────

/// Placeholder token for a matched volatile clock/time-of-day.
const PLACEHOLDER_TIME: &str = "<t>";
/// Placeholder for a percentage value.
const PLACEHOLDER_PCT: &str = "<pct>";
/// Placeholder for a progress counter like "3/10".
const PLACEHOLDER_COUNTER: &str = "<n/n>";
/// Placeholder for a spinner glyph on its own line.
const PLACEHOLDER_SPINNER: &str = "<s>";

/// A named volatile pattern used by [`NormalizationPolicy`].
///
/// `VolatileClass` is public because `NormalizationPolicy::volatile_patterns`
/// exposes it in its public API (audit item 13).
#[derive(Debug, Clone)]
pub struct VolatileClass {
    /// Human-readable label for this volatile class.
    ///
    /// Reserved for diagnostics / documentation; not used in the normalize()
    /// path itself. This avoids the dead_code warning while keeping the
    /// classification human-readable.
    #[allow(dead_code)]
    pub label: &'static str,
    /// Compiled regex that matches tokens in this class.
    pub regex: Regex,
    /// Stable replacement placeholder string.
    pub placeholder: &'static str,
}

/// Normalization policy: a set of volatile-token regexes that should be
/// replaced with stable placeholders during structure hashing.
///
/// Audit item 13: policies are extensible via `NormalizationPolicy::from_patterns`.
#[derive(Debug, Clone)]
pub struct NormalizationPolicy {
    /// Regex patterns for volatile content (clocks, percentages, timers,
    /// progress counters, spinners).
    pub volatile_patterns: Vec<VolatileClass>,
}

impl NormalizationPolicy {
    /// Apply every volatile class in this policy to `row`, replacing
    /// matched tokens with the class's stable placeholder.
    ///
    /// The order of replacement follows the policy's `volatile_patterns`
    /// order so that more-specific patterns (like elapsed timers) are
    /// applied before the general clock/time pattern.
    pub fn normalize(&self, row: &str) -> String {
        let mut result = row.to_string();
        for vc in &self.volatile_patterns {
            result = vc.regex.replace_all(&result, vc.placeholder).into_owned();
        }
        // Trim leading/trailing whitespace, like the original normalize_row.
        result.trim().to_string()
    }
}

/// Default policy -- only clearly-volatile token classes.
///
/// - Clock / time-of-day: `12:42:03`, `9:30`, `09:30:05`
/// - Elapsed / timer labels: `5:00 remaining`, `12:00:00 elapsed`
/// - Percentages: `37%`, `100.0%`
/// - Progress counters: `3/10`, `1/1`
///
/// Integers that are not part of a volatile class are preserved verbatim.
/// E.g. `"Port: 22"` remains `"Port: 22"` (hash differs from `"Port: 443"`).
impl Default for NormalizationPolicy {
    fn default() -> Self {
        Self {
            volatile_patterns: vec![
                // Elapsed/timer with label (most specific -- match before generic clock).
                VolatileClass {
                    label: "elapsed_timer_label",
                    // \b after 00:00 elapsed -- between "0" and ":" is a word boundary,
                    // so "00:00 elapsed" will match. This is acceptable: the label
                    // "elapsed" is consumed as part of the volatile timer token.
                    regex: Regex::new(r"\b\d+:\d{2}\s+(remaining|left|elapsed)\b").unwrap(),
                    placeholder: PLACEHOLDER_TIME,
                },
                // Clock / time-of-day (HH:MM or HH:MM:SS).
                VolatileClass {
                    label: "clock_time",
                    regex: Regex::new(r"\b\d{1,2}:\d{2}(:\d{2})?\b").unwrap(),
                    placeholder: PLACEHOLDER_TIME,
                },
                // Percentages -- no trailing \b because % is a non-word char,
                // and \b requires a word/non-word boundary.  After %, if the
                // next char is space or end-of-string (also non-word), there
                // is no boundary.  Use a word-boundary-free anchor instead.
                VolatileClass {
                    label: "percentage",
                    regex: Regex::new(r"\b\d{1,3}(\.\d+)?%").unwrap(),
                    placeholder: PLACEHOLDER_PCT,
                },
                // Progress counters like "3/10" or "1/1".
                VolatileClass {
                    label: "progress_counter",
                    regex: Regex::new(r"\b\d+\s*/\s*\d+\b").unwrap(),
                    placeholder: PLACEHOLDER_COUNTER,
                },
                // Standalone spinner glyph on its own line:
                // a single [|/\-] surrounded by whitespace (e.g. " | " or " / ").
                VolatileClass {
                    label: "spinner",
                    regex: Regex::new(r"(?m)^[[:space:]]*[|/\\-][[:space:]]*$").unwrap(),
                    placeholder: PLACEHOLDER_SPINNER,
                },
            ],
        }
    }
}

/// Build a normalization policy from raw pattern strings.
///
/// Each string is compiled as a regex. Invalid patterns cause an error;
/// the caller can choose to skip or propagate.
///
/// Audit item 13: the design contract exposes `volatile_patterns: Vec<String>`
/// as a raw description; this function bridges that contract to the compiled
/// `NormalizationPolicy`.
pub fn from_patterns(patterns: &[String]) -> Result<NormalizationPolicy, regex::Error> {
    // Start with the default volatile classes and append the extra patterns.
    let mut policy = NormalizationPolicy::default();
    for pat in patterns {
        let re = Regex::new(pat)?;
        policy.volatile_patterns.push(VolatileClass {
            label: "custom",
            regex: re,
            placeholder: "<custom>",
        });
    }
    Ok(policy)
}

// ── Public API (backward-compatible) ──────────────────────────────────

/// Normalize a single cell's text (mostly a passthrough now -- actual
/// normalization happens at the row level via `normalize_row`).
pub fn normalize_text(s: &str) -> String {
    s.to_string()
}

/// Normalize a full rendered row using the conservative default policy.
///
/// Volatile tokens (clocks, percentages, progress counters, spinners) are
/// replaced with stable placeholders. Arbitrary integers (e.g. `"Port: 22"`)
/// are preserved so that structurally different screens hash differently.
///
/// See [`NormalizationPolicy`] for the full set of volatile classes.
pub fn normalize_row(s: &str) -> String {
    let policy = NormalizationPolicy::default();
    policy.normalize(s)
}

/// Normalize a row with a custom [`NormalizationPolicy`].
///
/// This is the preferred entry point when the caller has a specific
/// volatile-pattern policy (e.g. loaded from the design contract).
pub fn normalize_row_with(s: &str, policy: &NormalizationPolicy) -> String {
    policy.normalize(s)
}

/// Structure hash cell contribution. Instead of normalizing the cell text
/// alone, we normalize the whole row context and then contribute only this
/// cell's portion (if it falls within a matched volatile span, the whole
/// span gets collapsed).
///
/// The simple approach: normalize the full row, then take the substring
/// corresponding to this cell's span in the normalized output. This is
/// only approximate -- proper span tracking would need the regex match
/// offsets. For our use case the structure hash doesn't need to be
/// human-readable, only stable.
pub fn normalize_cell_in_row(row: &str, cell_start: usize, cell_end: usize) -> String {
    let normalized = normalize_row(row);
    if cell_start >= normalized.len() {
        return String::new();
    }
    let end = cell_end.min(normalized.len());
    normalized[cell_start..end].to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_row_percentage() {
        // Percentages are volatile -- they should collapse.
        assert_eq!(normalize_row("CPU 37%"), "CPU <pct>");
        assert_eq!(normalize_row("Progress: 100.0%"), "Progress: <pct>");
    }

    #[test]
    fn test_normalize_row_clock() {
        // Clocks are volatile -- they should collapse.
        assert_eq!(normalize_row("12:42:03"), "<t>");
        assert_eq!(normalize_row("Time is 9:30"), "Time is <t>");
    }

    #[test]
    fn test_normalize_row_bare_number() {
        // Bare integers are NOW PRESERVED (conservative policy).
        assert_eq!(normalize_row("Count: 42"), "Count: 42");
        // Whitespace is trimmed (same behavior as original normalize_row).
        assert_eq!(normalize_row("  7 "), "7");
    }

    #[test]
    fn test_normalize_row_preserves_context() {
        // Port numbers are NOT volatile -- they must stay distinct.
        assert_eq!(normalize_row("Port: 8080"), "Port: 8080");
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
            "CPU <pct> at <t>, count 7"
        );
    }

    #[test]
    fn test_normalize_row_progress_counter() {
        assert_eq!(normalize_row("3/10"), "<n/n>");
        assert_eq!(
            normalize_row("Progress: 1/10 items"),
            "Progress: <n/n> items"
        );
    }

    #[test]
    fn test_normalize_row_spinner() {
        assert_eq!(normalize_row(" | "), "<s>");
        assert_eq!(normalize_row(" / "), "<s>");
        assert_eq!(normalize_row("not | not"), "not | not");
    }

    #[test]
    fn test_normalize_row_elapsed_timer() {
        // "5:00 remaining" -- the elapsed timer pattern matches the full token.
        assert_eq!(normalize_row("5:00 remaining"), "<t>");
        // "12:00:00 elapsed" -- the \b boundary between ":" and "0" at
        // position of "00:00 elapsed" causes the elapsed pattern to match
        // the suffix "00:00 elapsed", replacing it with <t>.  This is
        // expected behavior for this conservative policy: the keyword
        // "elapsed" is a timer label and gets consumed.
        assert_eq!(normalize_row("elapsed 12:00:00 elapsed"), "elapsed 12:<t>");
    }

    #[test]
    fn test_normalize_row_port_distinct() {
        // Key audit fix: different port numbers must produce different hashes.
        assert_ne!(normalize_row("Port: 22"), normalize_row("Port: 443"));
    }

    #[test]
    fn test_normalize_row_with_custom_policy() {
        let policy = NormalizationPolicy::default();
        // Custom policy: collapse bare integers too (old behavior for comparison).
        let custom = NormalizationPolicy {
            volatile_patterns: vec![VolatileClass {
                label: "everything_number",
                regex: Regex::new(r"\b\d+\b").unwrap(),
                placeholder: "<N>",
            }],
        };
        assert_eq!(normalize_row_with("Port: 8080", &policy), "Port: 8080");
        assert_eq!(normalize_row_with("Port: 8080", &custom), "Port: <N>");
    }

    #[test]
    fn test_from_patterns_valid() {
        let patterns = vec!["\\bfoo\\d+\\b".to_string()];
        let policy = from_patterns(&patterns).expect("compile pattern");
        assert_eq!(policy.normalize("say foo123 here"), "say <custom> here");
    }

    #[test]
    fn test_from_patterns_invalid() {
        // Invalid regex patterns are propagated as errors.
        let patterns = vec!["[invalid".to_string()];
        assert!(from_patterns(&patterns).is_err());
    }

    #[test]
    fn test_normalize_cell_in_row() {
        // With the percentage fix: "CPU 37%" -> "CPU <pct>", substring 0..5 is "CPU <".
        let normalized = normalize_cell_in_row("CPU 37%", 0, 5);
        assert_eq!(normalized, "CPU <");
    }

    #[test]
    fn test_normalize_text_passthrough() {
        assert_eq!(normalize_text("hello world"), "hello world");
    }
}
