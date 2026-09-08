//! Findings ledger and diagnostic (repair) packet assembly.
//!
//! Impl-family extraction (Phase 1): the `RunContext` struct and its
//! fields stay in `super`; this child module only hosts the method
//! bodies for this subsystem. Signatures, visibility, and callers are
//! unchanged.

use super::*;

impl RunContext {
    /// Append findings from an audit pass (typed; audit item 60's foundation).
    pub fn extend_findings(&mut self, findings: Vec<crate::audit::Finding>) -> anyhow::Result<()> {
        self.ensure_open()?;
        // Instance-unique ids (re-review P1 item 31): rule stays in
        // `rule_id`, the instance id gains a stable (rule, target)
        // discriminator, so two findings from one rule can never collide.
        self.findings
            .push_all(findings.into_iter().map(|f| f.instance()));
        Ok(())
    }

    /// Ingest findings, attaching probable source loci (W2.10) where the
    /// app's own coverage declarations can name them. Two joins, tiered by
    /// provenance (review §4): a matched widget-target entry that itself
    /// carries app-declared loci joins those as `attested` (native id →
    /// coverage event → locus — the app drew every edge); otherwise the
    /// run-level file-locus keys join as `correlated` (investigative,
    /// below the actionable fence). Findings whose evidence names no
    /// covered control keep `source_refs` empty — a locus is carried,
    /// never guessed.
    pub fn extend_findings_with_source_refs(
        &mut self,
        findings: Vec<crate::audit::Finding>,
    ) -> anyhow::Result<()> {
        self.ensure_open()?;
        // Coverage targets that are real file loci.
        let file_targets: Vec<&String> = self
            .coverage_ledger()
            .keys()
            .filter(|t| target_is_file_locus(t))
            .collect();
        // Widget-ish targets (widget:<id>, #id, bare id) → the app declared
        // coverage for that component in the same channel.
        let widget_targets: Vec<&String> = self
            .coverage_ledger()
            .keys()
            .filter(|t| !target_is_file_locus(t))
            .collect();
        if file_targets.is_empty() && widget_targets.is_empty() {
            self.findings.push_all(findings);
            return Ok(());
        }
        let mut enriched = findings;
        for f in &mut enriched {
            if !f.source_refs.is_empty() {
                continue; // producer already attached loci; never overwrite
            }
            let mut refs: Vec<crate::semantic::source_ref::SourceRef> = Vec::new();
            for ev in &f.evidence {
                let target = ev.target.as_deref().unwrap_or("");
                let Some(matched_widget) = widget_targets
                    .iter()
                    .find(|w| coverage_target_matches_control(w, target))
                else {
                    continue;
                };
                // Attested first: the matched entry's own app-declared loci.
                if let Some(entry) = self.coverage_ledger().get(*matched_widget) {
                    refs.extend(entry.source_refs.iter().cloned().map(|mut sr| {
                        sr.provenance = crate::semantic::source_ref::Provenance::Attested;
                        sr
                    }));
                }
                if !refs.is_empty() {
                    break;
                }
                // Correlated fallback: file loci declared elsewhere in the
                // same run — all candidate cause sites, never attested for
                // THIS finding; per-locus confidence stays modest because
                // the app did not scope to this finding.
                for ft in &file_targets {
                    if let Some(sr) = source_ref_from_target(ft) {
                        refs.push(sr);
                    }
                }
                break;
            }
            if !refs.is_empty() {
                // Deduplicate by location.
                refs.dedup_by(|a, b| a.location() == b.location());
                f.source_refs = refs;
            }
        }
        self.findings.push_all(enriched);
        Ok(())
    }

    /// Findings accumulated in this run. Round-2 (G1): delegates to
    /// `super::finding_store::FindingStore`.
    pub fn findings(&self) -> &[crate::audit::Finding] {
        self.findings.all()
    }

    /// Wave 5 item 45: the explain-time source join, as a pure lookup.
    /// A finding that stored with no loci gains any loci the coverage
    /// ledger can attest for its evidence targets — coverage events often
    /// arrive AFTER the audit pass that produced the finding, and the
    /// explanation surface should see the join without mutating stored
    /// findings. Findings that already carry loci (or whose evidence names
    /// no covered control) come back unchanged.
    pub fn join_source_refs_if_known(
        &self,
        finding: &crate::audit::Finding,
    ) -> crate::audit::Finding {
        if !finding.source_refs.is_empty() {
            return finding.clone();
        }
        // App-attested loci (item 41) outrank inferred ones: an entry that
        // carries a native `source` field is the app's own claim about
        // where that widget lives. Ledger KEYS that are file loci are the
        // weaker, correlational join and only fill gaps.
        let widget_targets: Vec<&String> = self
            .coverage_ledger()
            .keys()
            .filter(|t| !target_is_file_locus(t))
            .collect();
        if widget_targets.is_empty() {
            return finding.clone();
        }
        let mut refs: Vec<crate::semantic::source_ref::SourceRef> = Vec::new();
        for ev in &finding.evidence {
            let target = ev.target.as_deref().unwrap_or("");
            let Some(w) = widget_targets
                .iter()
                .find(|w| coverage_target_matches_control(w, target))
            else {
                continue;
            };
            // Prefer the app-attested loci stored on the matched entry.
            // This is the direct chain the provenance tier calls attested:
            // the finding's control matched a coverage target the app
            // declared a native id AND a source locus for (review §4).
            if let Some(entry) = self.coverage_ledger().get(*w) {
                refs.extend(entry.source_refs.iter().cloned().map(|mut sr| {
                    sr.provenance = crate::semantic::source_ref::Provenance::Attested;
                    sr
                }));
                if !entry.source_refs.is_empty() {
                    continue;
                }
            }
            // Fall back to file-locus keys. This is a RUN-LEVEL correlation
            // — the finding shares identity with coverage whose loci the
            // app declared elsewhere — so it joins as `correlated`, never
            // attested, and sits below the actionable fence (review §4: a
            // 0.7 float must not turn correlation into a cause site).
            for ft in self
                .coverage_ledger()
                .keys()
                .filter(|t| target_is_file_locus(t))
            {
                if let Some(mut sr) = source_ref_from_target(ft) {
                    sr.provenance = crate::semantic::source_ref::Provenance::Correlated;
                    refs.push(sr);
                }
            }
            break;
        }
        if refs.is_empty() {
            return finding.clone();
        }
        refs.dedup_by(|a, b| a.location() == b.location());
        let mut joined = finding.clone();
        joined.source_refs = refs;
        joined
    }

    /// Assemble [`crate::audit::repair::DiagnosticContext`]s for every
    /// finding (review §2/§3, formerly `repair_packets`/`RepairPacket`).
    /// Each context joins the finding with its replayable reproduction
    /// (when one exists), its provenance-tiered source loci, a
    /// verification plan (targeted checks + optional replay leg), and
    /// observation-shaped next steps — everything an agent needs to
    /// investigate the finding without re-deriving the parts. This
    /// surface increases knowledge; it does not prescribe edits. Findings
    /// that cannot form a context (no evidence) are skipped and counted,
    /// never silently dropped from the list.
    pub fn diagnostic_contexts(&self) -> (Vec<crate::audit::repair::DiagnosticContext>, usize) {
        let sessions: Vec<String> = self.sessions.session_ids();
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for f in self.findings.all() {
            // Late-join source loci (review P1 item 14): coverage events often
            // arrive AFTER the audit pass that produced this finding, so a
            // finding stored without loci gains them here if the coverage
            // ledger can now attest them. The context then points the
            // agent at the candidate source, not at nothing.
            let joined = if f.source_refs.is_empty() {
                self.join_source_refs_if_known(f)
            } else {
                f.clone()
            };
            let repro_ids: Vec<String> = {
                let all = self.scenarios.all();
                let mut counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for sc in all.values() {
                    *counts.entry(sc.name.as_str()).or_insert(0) += 1;
                }
                let mut ids: Vec<String> = all.keys().cloned().collect();
                for sc in all.values() {
                    if counts.get(sc.name.as_str()).copied().unwrap_or(0) == 1 {
                        ids.push(sc.name.clone());
                    }
                }
                ids
            };
            // The loader resolves by id first, then unambiguous name.
            let loader = |id: &str| {
                let _ = &repro_ids;
                self.load_scenario(id).ok()
            };
            let ctx = crate::audit::repair::DiagnosticContext::assemble(
                joined.clone(),
                &self.id(),
                sessions.clone(),
                loader,
            );
            match ctx {
                Some(mut c) => {
                    // Carry the (late-joined) finding's loci onto the context
                    // (the assemble API leaves them for the caller to avoid
                    // duplication).
                    c.source_refs = joined.source_refs;
                    out.push(c);
                }
                None => skipped += 1,
            }
        }
        (out, skipped)
    }
}
