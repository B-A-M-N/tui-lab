//! Beta-audit P1.2: explicit verification strategies for findings.
//!
//! `tui_workflow action=verify` used to map a finding's category slug
//! through `AuditProfile::parse` and fall back to the `full` composite
//! when the slug was not a profile name — so a `contract/…` or
//! orchestration finding could trigger a completely unrelated broad
//! audit. That fallback is gone. Instead every finding resolves through
//! an explicit strategy table: a rule-prefix → audit-profile mapping
//! where the profile is the surface that ACTUALLY re-evaluates the
//! rule, plus the risk class and whether the strategy needs the
//! original reproduction replayed first. A finding whose rule has no
//! entry verifies through its evidence targets only (targeted
//! re-observation), never through a guessed broad profile.

use crate::audit::orchestrator::AuditProfile;

/// How a finding's rule can be re-checked after a fix.
#[derive(Debug, Clone)]
pub struct VerificationStrategy {
    /// The audit surface that re-evaluates this rule. `None` = no
    /// engine surface exists for the rule (verify works through the
    /// reproduction replay and targeted observation only).
    pub audit_profile: Option<AuditProfile>,
    /// Whether the verification needs the finding's recorded
    /// reproduction replayed before the re-check (setup, not proof).
    pub needs_reproduction: bool,
    /// Why this strategy: the rule-prefix match (or the absence that
    /// produced the conservative default).
    pub matched_on: &'static str,
}

impl VerificationStrategy {
    /// Resolve the strategy for a finding: by RULE (the `rule_id` when
    /// present, else the finding id's rule prefix), falling back to the
    /// finding's category only when that category names an actual audit
    /// profile — never to a broad composite.
    pub fn for_finding(finding: &crate::audit::Finding) -> Self {
        let rule_key = finding
            .rule_id
            .clone()
            .unwrap_or_else(|| finding.id.clone());
        Self::for_rule(&rule_key, finding)
    }

    fn for_rule(rule_key: &str, finding: &crate::audit::Finding) -> Self {
        // Rule prefixes → the profile that re-evaluates the rule. The
        // prefix is the rule's id before the first `-`
        // (`KB-TRAP` → `KB`), matched case-sensitively against the
        // audit families' own prefixes.
        let prefix = rule_key.split('-').next().unwrap_or("");
        let entry = match prefix {
            "KB" => Some((AuditProfile::Keyboard, "KB (keyboard traversal)")),
            "FOCUS" => Some((AuditProfile::Focus, "FOCUS (focus visibility)")),
            "LAYOUT" | "RESIZE" => Some((AuditProfile::Resize, "LAYOUT (resize matrix)")),
            "CLIP" => Some((AuditProfile::Clipping, "CLIP (clipping)")),
            "DISC" => Some((AuditProfile::Discoverability, "DISC (discoverability)")),
            "NAV" => Some((AuditProfile::Navigation, "NAV (navigation)")),
            "CONTRACT" => Some((AuditProfile::Contract, "CONTRACT (conformance)")),
            "COLOR" => Some((AuditProfile::Color, "COLOR")),
            "PERF" => Some((AuditProfile::Performance, "PERF (performance)")),
            "MOUSE" => Some((AuditProfile::Mouse, "MOUSE")),
            "STATE" => Some((AuditProfile::States, "STATE (states driver)")),
            "ERR" => Some((AuditProfile::Errors, "ERR (errors driver)")),
            "UNI" | "UNICODE" => Some((AuditProfile::Unicode, "UNI (unicode)")),
            "CTRL" => Some((AuditProfile::Controls, "CTRL (coverage)")),
            "TM" | "MODES" => Some((AuditProfile::TerminalModes, "TM (terminal modes)")),
            "RENDER" => Some((AuditProfile::Rendering, "RENDER (rendering)")),
            "IP" | "PROTO" => Some((AuditProfile::InputProtocol, "IP (input protocol)")),
            "SHELL" => Some((AuditProfile::ShellCli, "SHELL (shell/cli)")),
            "LC" | "LIFECYCLE" => Some((AuditProfile::Lifecycle, "LC (lifecycle modes)")),
            "QR" | "CPR" => Some((AuditProfile::QueryResponse, "QR (query/response)")),
            _ => None,
        };
        if let Some((profile, matched_on)) = entry {
            return VerificationStrategy {
                audit_profile: Some(profile),
                needs_reproduction: finding.reproduction.is_some(),
                matched_on,
            };
        }
        // No rule entry: the category names an actual profile only when
        // its slug parses — `keyboard`, `resize`, … resolve; `contract/ui`,
        // `orchestration`, `audit` do NOT (the old code degraded those to
        // `full` — an unrelated broad audit — which is exactly the bug).
        let cat_profile = AuditProfile::parse(finding.category.as_str()).ok();
        let matched_on = if cat_profile.is_some() {
            "category-names-an-audit-profile"
        } else {
            "no-rule-entry-category-is-not-a-profile"
        };
        VerificationStrategy {
            audit_profile: cat_profile,
            needs_reproduction: finding.reproduction.is_some(),
            matched_on,
        }
    }
}

/// One recorded verification (beta-audit P1.2): what was verified, how,
/// and what the evidence says — persisted with the run so a later caller
/// can CITE the verification instead of re-deriving it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerificationRecord {
    /// The finding's canonical fingerprint (rule + category + evidence
    /// targets, order-insensitive).
    pub finding_fingerprint: String,
    /// The finding id at verification time.
    pub finding_id: String,
    /// Session + generation the verification ran against.
    pub session: String,
    pub generation: u32,
    /// The strategy that ran (profile name, or the conservative default's
    /// name when no engine surface exists).
    pub strategy: String,
    /// Replay leg result: "ran" | "skipped" | "unavailable".
    pub replay: String,
    /// Re-check leg result: "refired" | "clean" | "gated" | "skipped" | "no_surface".
    pub recheck: String,
    /// The workflow-level verdict.
    pub verdict: String,
    /// Evidence refs: frame ids, scenario id, audit pass reference.
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// Unix-millis.
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str, rule_id: Option<&str>, category: &str) -> crate::audit::Finding {
        crate::audit::Finding {
            id: id.into(),
            rule_id: rule_id.map(str::to_string),
            severity: crate::audit::Severity::Warn,
            category: crate::audit::Category::parse(category),
            summary: format!("test finding {id}"),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                "button/save",
                "evidence",
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }
    }

    /// Rule prefixes map to the profile that actually re-evaluates them.
    #[test]
    fn rule_prefixes_select_their_own_surface() {
        let s = VerificationStrategy::for_finding(&finding("KB-TRAP", Some("KB-TRAP"), "keyboard"));
        assert!(
            matches!(s.audit_profile, Some(AuditProfile::Keyboard)),
            "{s:?}"
        );
        let s = VerificationStrategy::for_finding(&finding("CLIP-2", Some("CLIP-2"), "clipping"));
        assert!(
            matches!(s.audit_profile, Some(AuditProfile::Clipping)),
            "{s:?}"
        );
        let s = VerificationStrategy::for_finding(&finding(
            "DISC-001",
            Some("DISC-001"),
            "discoverability",
        ));
        assert!(
            matches!(s.audit_profile, Some(AuditProfile::Discoverability)),
            "{s:?}"
        );
    }

    /// THE P1.2 INVARIANT: a finding whose rule has no entry AND whose
    /// category is not a profile name gets NO surface — it is never
    /// degraded into an unrelated broad `full` audit.
    #[test]
    fn no_rule_entry_and_non_profile_category_never_falls_back_to_full() {
        for (id, category) in [
            ("AUDIT-RESIDUE", "audit"),
            ("ORCH-1", "orchestration"),
            ("WHATEVER-9", "contract/ui"),
        ] {
            let s = VerificationStrategy::for_finding(&finding(id, None, category));
            assert!(
                s.audit_profile.is_none(),
                "{id}/{category} must have NO re-check surface (old code ran `full`): {s:?}"
            );
            assert_eq!(s.matched_on, "no-rule-entry-category-is-not-a-profile");
        }
    }

    /// A category that names a REAL profile still resolves (the
    /// conservative default is not over-strict).
    #[test]
    fn category_that_names_a_profile_still_resolves() {
        let s = VerificationStrategy::for_finding(&finding("X-1", None, "color"));
        assert!(
            matches!(s.audit_profile, Some(AuditProfile::Color)),
            "{s:?}"
        );
        assert_eq!(s.matched_on, "category-names-an-audit-profile");
    }

    /// A rule-level match outranks the category: an `audit`-category
    /// finding carrying a KB- rule still verifies through the keyboard
    /// surface.
    #[test]
    fn rule_outranks_category() {
        let s = VerificationStrategy::for_finding(&finding("KB-TRAP", Some("KB-TRAP"), "audit"));
        assert!(
            matches!(s.audit_profile, Some(AuditProfile::Keyboard)),
            "{s:?}"
        );
    }
}
