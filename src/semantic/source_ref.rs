//! SourceRef — tie a semantic problem to a source file/line so the agent can
//! *repair* rather than merely report (W2.10). Every diagnostic that points at
//! code carries one of these; the confidence fence keeps the agent from
//! hallucinating a repair target.

/// A pointer into a source file, with the confidence that this locus is the
/// true cause site.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceRef {
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework_id: Option<String>,
    /// 0.0..=1.0 belief that this line is the cause site.
    pub confidence: f32,
    /// How the locus was established: "stack" | "runtime-sourcemap" |
    /// "static-analysis" | "framework-adapter" | "manual".
    pub source: String,
}

impl SourceRef {
    /// The canonical `file:line[:col]` string an editor/agent opens on.
    pub fn location(&self) -> String {
        match self.column {
            Some(col) => format!("{}:{}:{}", self.file, self.line, col),
            None => format!("{}:{}", self.file, self.line),
        }
    }

    /// A cause-site a repair pass should trust: the locus is both confident
    /// enough and from a source known to map to real lines (runtime/stack or
    /// sourcemap), not a guess.
    pub fn is_actionable(&self) -> bool {
        self.confidence >= 0.7
            && matches!(
                self.source.as_str(),
                "stack" | "runtime-sourcemap" | "framework-adapter"
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_renders_column_when_present() {
        let r = SourceRef {
            file: "src/main.rs".into(),
            line: 42,
            column: Some(17),
            symbol: None,
            framework_id: None,
            confidence: 0.9,
            source: "stack".into(),
        };
        assert_eq!(r.location(), "src/main.rs:42:17");
        assert!(r.is_actionable());
    }

    #[test]
    fn low_confidence_or_guess_is_not_actionable() {
        let r = SourceRef {
            file: "src/main.rs".into(),
            line: 7,
            column: None,
            symbol: None,
            framework_id: None,
            confidence: 0.4,
            source: "static-analysis".into(),
        };
        assert_eq!(r.location(), "src/main.rs:7");
        assert!(!r.is_actionable(), "a half-guess must not drive a repair");
    }
}