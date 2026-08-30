//! Confidence + evidence model for inferred objects.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Confidence {
    /// 0.0..=1.0 heuristic confidence.
    pub score: f32,
    /// Why we believe this (e.g. "reverse-video", "box-border-detected").
    pub evidence: Vec<String>,
    /// "inferred" | "native".
    pub source: String,
}

impl Confidence {
    pub fn inferred(score: f32, evidence: &[&str]) -> Self {
        Confidence {
            score,
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
            source: "inferred".into(),
        }
    }
    pub fn native() -> Self {
        Confidence {
            score: 1.0,
            evidence: vec!["framework-adapter".into()],
            source: "native".into(),
        }
    }
}
