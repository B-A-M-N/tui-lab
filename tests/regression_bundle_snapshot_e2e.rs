//! Beta-audit P0.10: regression bundles compare against the LATEST
//! AUDIT PASS — a first-class snapshot — never the cumulative finding
//! ledger.
//!
//! The defect: `tui_run bundle` compared the labeled baseline against
//! `run.findings()`, which deliberately accumulates EVERY finding ever
//! recorded in the run. A defect that was in the baseline, was FIXED,
//! and is absent from the newest audit pass still matched its stale
//! copy in the ledger — so the bundle reported `persisting` forever and
//! the "did the change hold?" packet lied about the fix.
//!
//! The invariant under test: after a fresh audit pass that no longer
//! contains the finding, bundle must report it FIXED (it is in the
//! baseline, absent from the current pass), with the ledger's stale
//! copy unable to resurrect it.

use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;
use tui_lab::mcp::TuiLabServer;

fn result_text(out: &rmcp::model::CallToolResult) -> String {
    out.content
        .first()
        .and_then(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn result_json(out: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&result_text(out)).unwrap_or(serde_json::Value::Null)
}

fn audit_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiAuditParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn run_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiRunParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn session_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiSessionParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

async fn start(server: &TuiLabServer, script: &str) -> String {
    let out = server
        .tui_session(session_params(json!({
            "action": "start", "command": "python3",
            "args": ["-c", script], "cols": 80, "rows": 24,
        })))
        .await;
    result_json(&out)["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string()
}

/// The end-to-end did-the-fix-hold scenario:
/// 1. keyboard audit pass → label `baseline` (finds real findings);
/// 2. a DIFFERENT-profile pass that does not emit the keyboard findings
///    becomes the latest pass (the fix landed);
/// 3. bundle against the baseline must classify the keyboard finding
///    FIXED — the stale copy in the cumulative ledger (still present,
///    since audits append) must not read `persisting`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bundle_reports_fixed_after_the_fix_not_persisting_from_the_ledger() {
    let server = TuiLabServer::new();
    let sid = start(&server, "print('bundle-fix'); input()").await;

    // Pass 1: the baseline — a keyboard audit (drives, hence consent).
    let first = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "keyboard", "label": "baseline",
                "id": sid, "allow_mutation": true,
            })))
            .await,
    );
    assert_eq!(first["category"], "success", "{first}");
    let baseline_findings = first["data"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !baseline_findings.is_empty(),
        "the keyboard pass emits findings to baseline: {first}"
    );
    let target = &baseline_findings[0];
    let target_id = target["id"].as_str().expect("finding id").to_string();

    // Pass 2 (the fix): a discoverability audit — a different profile
    // whose finding set does not include the keyboard defect. This pass
    // becomes the run's current snapshot.
    let second = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "discoverability", "id": sid,
            })))
            .await,
    );
    assert_eq!(second["category"], "success", "{second}");

    // The cumulative ledger still contains the keyboard finding
    // (audits append; history is kept) — that is the trap the old code
    // fell into.
    let findings_res = result_json(
        &server
            .tui_run(run_params(json!({ "action": "findings" })))
            .await,
    );
    let _ = findings_res; // ledger content is not asserted here; the bundle is.

    let bundle = result_json(
        &server
            .tui_run(run_params(json!({
                "action": "bundle", "finding_id": target_id,
                "compare_to": "baseline",
            })))
            .await,
    );
    assert_eq!(bundle["category"], "success", "{bundle}");
    let data = &bundle["data"];
    assert_eq!(
        data["comparable"], true,
        "a completed pass exists so the packet is comparable: {bundle}"
    );
    assert_eq!(data["current_set"], "latest_audit_pass", "{bundle}");
    let after = &data["after"];
    assert_eq!(
        after["verdict"], "fixed",
        "the defect is in the baseline and ABSENT from the newest pass — the ledger's stale copy must not read persisting: {bundle}"
    );
    // And the side-effect guard ran over the same pass snapshot.
    assert!(
        data["side_effects"]["count"].is_u64(),
        "side effects ride the packet: {bundle}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
}

/// A run with NO completed audit pass says so honestly: comparable=false
/// with guidance — never a comparison silently run against the ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bundle_without_a_pass_reports_not_comparable() {
    let server = TuiLabServer::new();
    let sid = start(&server, "print('bundle-nopass'); input()").await;

    // Put a finding in the LEDGER only (no audit pass recorded): use a
    // labeled audit then start a NEW run? Simpler: a run where no audit
    // has completed at all — bundle over any id is invalid_request; but
    // we need a finding to reach the comparable check. Drive one audit
    // in run A, then close+new, and bundle in run B (fresh ledger).
    let first = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "keyboard", "label": "baseline",
                "id": sid, "allow_mutation": true,
            })))
            .await,
    );
    let baseline = first["data"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let target_id = baseline[0]["id"].as_str().expect("finding id").to_string();

    // Swap to a fresh run: no completed pass in THIS run.
    let swapped = result_json(
        &server
            .tui_run(run_params(json!({ "action": "new", "discard": true })))
            .await,
    );
    assert!(
        swapped["data"]["new_run_id"].is_string(),
        "swap to fresh run: {swapped}"
    );

    let bundle = result_json(
        &server
            .tui_run(run_params(json!({
                "action": "bundle", "finding_id": target_id,
                "compare_to": "baseline",
            })))
            .await,
    );
    assert_eq!(bundle["category"], "success", "{bundle}");
    assert_eq!(
        bundle["data"]["comparable"], false,
        "no completed pass in this run ⇒ honestly not comparable: {bundle}"
    );
    assert!(
        bundle["data"]["note"]
            .as_str()
            .unwrap_or("")
            .contains("tui_audit"),
        "the note names how to build a comparable run: {bundle}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
}
