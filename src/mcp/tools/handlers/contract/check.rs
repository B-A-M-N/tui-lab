//! Contract conformance-check plumbing (god-object round 2, G5c):
//! moved OUT of the `TuiLabServer` impl and file scope — the actor-
//! backed check drives a session and records findings, which is
//! contract-domain work, not server plumbing. The server borrow is a
//! parameter now.

use crate::error::ErrorCategory;
use crate::mcp::helpers::{err, lease_refused, ok};
use rmcp::serde_json::json;

/// Actor-backed conformance check with run-ledger recording.
pub(crate) async fn check_contract_against(
    s: &crate::mcp::tools::TuiLabServer,
    id: Option<&str>,
    contract: crate::design::ProjectContract,
    mode_override: Option<crate::design::ContractMode>,
) -> rmcp::model::CallToolResult {
    let selector = id.map(str::to_string);
    let run = s.run.clone();
    let mode = mode_override;
    match s
        .with_sess(selector.as_deref(), move |sess| {
            // Conformance drives the app (declared keys, resizes,
            // Escape/Tab probes); the human control lease (item 76)
            // refuses it like any other driving path. The closure's
            // Result discriminates lease-refusal (left) from the
            // engine result (right).
            if let Some(refused) = lease_refused(sess) {
                return Err(refused);
            }
            Ok(crate::design::check_contract_with_mode(
                sess, &contract, mode,
            ))
        })
        .await
    {
        Ok(Ok(Ok(report))) => {
            // Findings feed the run ledger (item 49). Baselines are
            // recorded ONLY by the explicit `action=baseline` (re-review
            // item 31): a silent auto-record under "baseline" made every
            // status check clobber the comparison point.
            let findings = report.findings();
            let summary = report.summary();
            let results = report.results.clone();
            let mut run = run.lock().unwrap();
            let _ = run.extend_findings_with_source_refs(findings);
            ok(json!({
                "verdict": report.verdict.as_str(),
                "summary": summary,
                "results": results,
            }))
        }
        Ok(Ok(Err(e))) => err(ErrorCategory::BackendError, e.to_string()),
        Ok(Err(refused)) => refused,
        Err(e) => e,
    }
}

/// The check without ledger recording (compare runs its own baseline
/// bookkeeping and needs the raw report).
pub(crate) async fn check_contract_inner(
    s: &crate::mcp::tools::TuiLabServer,
    id: Option<&str>,
    contract: crate::design::ProjectContract,
    mode_override: Option<crate::design::ContractMode>,
) -> Result<anyhow::Result<crate::design::ContractReport>, rmcp::model::CallToolResult> {
    let selector = id.map(str::to_string);
    let mode = mode_override;
    match s
        .with_sess(selector.as_deref(), move |sess| {
            // Same lease rule as check_contract_against (item 76).
            if let Some(refused) = lease_refused(sess) {
                return Err(refused);
            }
            Ok(crate::design::check_contract_with_mode(
                sess, &contract, mode,
            ))
        })
        .await
    {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(refused)) => Err(refused),
        Err(e) => Err(e),
    }
}

/// Parse the `mode` tool parameter; `None` means "use the contract's own
/// mode". Unknown strings are `None` here because the typed enum surfaces
/// them as invalid_request at the parameter layer.
/// Item 32: resolve the typed mode selector into the engine's mode
/// override. `None` (absent) means "use the contract document's mode";
/// `Known::Other` is the caller's typo and names the accepted set.
pub(crate) fn contract_mode_override(
    mode: &Option<crate::mcp::params::Known<crate::mcp::params::ContractModeParam>>,
) -> Result<Option<crate::design::ContractMode>, String> {
    use crate::mcp::params::{ContractModeParam as CMP, Known};
    match mode {
        None => Ok(None),
        Some(Known::Known(CMP::Advisory)) => Ok(Some(crate::design::ContractMode::Advisory)),
        Some(Known::Known(CMP::Validation)) => Ok(Some(crate::design::ContractMode::Validation)),
        Some(Known::Known(CMP::Strict)) => Ok(Some(crate::design::ContractMode::Strict)),
        Some(Known::Other(s)) => Err(format!(
            "unknown mode '{s}': expected one of advisory, validation, strict"
        )),
    }
}

/// One (key, before, after) row per changed check in a contract comparison.
pub(crate) type ContractCheckDiff = Vec<(
    String,
    crate::design::CheckResult,
    crate::design::CheckResult,
)>;

/// Diff two contract reports result-by-result, keyed by `group + name`.
/// Regressions: Pass→Fail (and Pass→Warn for required checks). Fixed:
/// Fail→Pass, Warn→Pass. Verdict-neutral changes (Warn→Fail on optional
/// checks etc.) are reported as regressions too — stricter is a regression
/// whenever the check was required.
pub(crate) fn diff_contract_reports(
    base: &crate::design::ContractReport,
    current: &crate::design::ContractReport,
) -> (ContractCheckDiff, ContractCheckDiff) {
    use crate::design::{CheckResult, Verdict};
    let key = |r: &CheckResult| format!("{}/{}", r.group, r.name);
    let mut regressions = Vec::new();
    let mut fixed = Vec::new();
    for cur in &current.results {
        let Some(prev) = base.results.iter().find(|r| key(r) == key(cur)) else {
            continue; // new check, no history
        };
        let worsened = match (prev.verdict, cur.verdict) {
            (Verdict::Pass, Verdict::Fail) => true,
            (Verdict::Pass, Verdict::Warn) => cur.required,
            (Verdict::Warn, Verdict::Fail) => cur.required,
            // Item 32: Unverified/Unsupported are not failures — moving
            // into them is a loss of evidence, surfaced separately, never
            // counted as a regression (which would punish the harness for
            // its own blind spots).
            _ => false,
        };
        let improved = matches!(
            (prev.verdict, cur.verdict),
            (Verdict::Fail, Verdict::Pass)
                | (Verdict::Warn, Verdict::Pass)
                | (Verdict::Fail, Verdict::Warn)
        );
        if worsened {
            regressions.push((key(cur), prev.clone(), cur.clone()));
        } else if improved {
            fixed.push((key(cur), prev.clone(), cur.clone()));
        }
    }
    (regressions, fixed)
}
