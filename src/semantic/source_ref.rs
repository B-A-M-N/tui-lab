//! SourceRef — tie a semantic problem to a source file/line so the agent can
//! *investigate* rather than merely report (W2.10). Every diagnostic that
//! points at code carries one of these; the PROVENANCE tier keeps the agent
//! from treating a weak correlation as a cause site (review §4: a
//! floating-point threshold alone was letting a 0.7-confidence run-level
//! correlation present as an actionable repair target).

/// How the link between THIS finding/locus pair was established. Provenance,
/// not confidence: a number says how strongly the locus maps to real code;
/// this says how directly it was tied to the problem (review §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Provenance {
    /// A run-level correlation: the finding involves a control that shares
    /// identity with coverage targets whose file loci the app declared
    /// elsewhere. Investigative evidence — genuinely useful, never
    /// cause-site-grade.
    Correlated,
    /// Derived by the engine from naming/layout heuristics (e.g. a parsed
    /// `file.rs:42` string with no app declaration behind it).
    Inferred,
    /// A direct mapping chain: the app's own native id for the component →
    /// the same component's coverage/native event → the app-declared source
    /// locus. The app drew the line; we only joined the endpoints. Set only
    /// where the chain is real — never as a default.
    Attested,
    /// Nothing is known about how the locus was established (legacy
    /// persisted data, the serde default).
    #[default]
    Unknown,
}

impl Provenance {
    /// Stable wire name.
    pub fn name(&self) -> &'static str {
        match self {
            Provenance::Attested => "attested",
            Provenance::Correlated => "correlated",
            Provenance::Inferred => "inferred",
            Provenance::Unknown => "unknown",
        }
    }

    /// Parse a provenance name (persisted runs round-trip through this).
    pub fn parse(s: &str) -> Self {
        match s {
            "attested" => Provenance::Attested,
            "correlated" => Provenance::Correlated,
            "inferred" => Provenance::Inferred,
            _ => Provenance::Unknown,
        }
    }
}

/// A pointer into a source file, with the provenance that this locus is the
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
    /// How directly the locus was tied to the finding (review §4). Absent
    /// in persisted pre-tier data → Unknown, which is never actionable.
    #[serde(default)]
    pub provenance: Provenance,
}

impl SourceRef {
    /// The canonical `file:line[:col]` string an editor/agent opens on.
    pub fn location(&self) -> String {
        match self.column {
            Some(col) => format!("{}:{}:{}", self.file, self.line, col),
            None => format!("{}:{}", self.file, self.line),
        }
    }

    /// A cause-site the agent may treat as "open this first": the locus is
    /// directly attested for THIS finding (not merely correlated at run
    /// level), confident enough, and from a source known to map to real
    /// lines. Provenance gates; confidence only refines (review §4:
    /// a 0.7 float must not turn correlation into actionability).
    pub fn is_actionable(&self) -> bool {
        self.provenance == Provenance::Attested
            && self.confidence >= 0.7
            && matches!(
                self.source.as_str(),
                "stack" | "runtime-sourcemap" | "framework-adapter"
            )
    }
}

/// Re-review P1 item 28: one identity joining the several ids a component
/// accumulates across the system — the semantic node id (what observation
/// sees), the native id (what the app calls it), the contract component
/// name (what the design contract requires), and the source loci (where the
/// code lives). Attached to semantic nodes so a repair pass can move from
/// "the rendered Save button is clipped" to "edit src/ui/settings.rs:184"
/// without another 5-8 lookups.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ComponentIdentity {
    /// The semantic node/control id (`button/save`, `#save-button`).
    pub semantic_id: String,
    /// The app-declared native id, when the side-channel carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    /// The design-contract component name, when a contract names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_id: Option<String>,
    /// Source loci, best first. Empty when nothing attests a location.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<SourceRef>,
}

impl ComponentIdentity {
    /// Identity from a semantic id alone (the floor — every node has one).
    pub fn semantic(semantic_id: impl Into<String>) -> Self {
        ComponentIdentity {
            semantic_id: semantic_id.into(),
            ..Default::default()
        }
    }

    /// Join a native id + its source locus into the identity.
    pub fn with_native(mut self, native_id: impl Into<String>) -> Self {
        self.native_id = Some(native_id.into());
        self
    }

    /// Attach source loci (replacing any).
    pub fn with_source_refs(mut self, refs: Vec<SourceRef>) -> Self {
        self.source_refs = refs;
        self
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
            provenance: Provenance::Attested,
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
            provenance: Provenance::Attested,
        };
        assert_eq!(r.location(), "src/main.rs:7");
        assert!(!r.is_actionable(), "a half-guess must not drive a repair");
    }

    #[test]
    fn correlated_is_never_actionable_even_at_the_confidence_fence() {
        // Review §4's exact defect: a run-level correlation at confidence
        // 0.7 from a framework-adapter source used to clear the old numeric
        // fence. Provenance gates first.
        let r = SourceRef {
            file: "src/widgets.rs".into(),
            line: 10,
            column: None,
            symbol: None,
            framework_id: None,
            confidence: 0.7,
            source: "framework-adapter".into(),
            provenance: Provenance::Correlated,
        };
        assert!(
            !r.is_actionable(),
            "a run-level correlation is investigative, not a cause site"
        );
        // Unknown (legacy persisted data) is likewise fenced.
        let legacy = SourceRef {
            provenance: Provenance::Unknown,
            ..r.clone()
        };
        assert!(!legacy.is_actionable());
    }
}
