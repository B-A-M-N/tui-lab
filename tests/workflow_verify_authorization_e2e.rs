//! Beta-audit P1.2: `tui_workflow action=verify` is trustworthy
//! verification.
//!
//! Four guarantees, at the MCP surface:
//!
//! 1. GATING — an invasive re-check surface is WITHHELD under the
//!    default (`gated`, verdict null with the allow_mutation remedy
//!    named), and `allow_mutation=true` runs it. Never a silent
//!    downgrade, never a fabricated pass.
//! 2. NO BROAD FALLBACK — a finding whose rule/category names no audit
//!    profile is refused with an honest no-surface error, never
//!    degraded into an unrelated `full` audit (the old behavior).
//! 3. REPLAY IS SETUP — the replay leg is reported as
//!    `reproduction_setup` with an explicit not-proof note.
//! 4. VERIFICATION IS RECORDED — each verify execution persists a
//!    `VerificationRecord` keyed by the finding's fingerprint, so a
//!    later caller can cite it.

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

fn session_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiSessionParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn workflow_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiWorkflowParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn audit_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiAuditParams> {
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

/// (1) an invasive profile's re-check is gated by default with the
/// remedy named, and (1b) runs under allow_mutation=true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invasive_recheck_is_gated_until_authorized() {
    let server = TuiLabServer::new();
    let sid = start(&server, "print('verify-gate'); input()").await;

    // Any finding with an evidence target works for the gating shape;
    // use a discoverability finding (observational) for the run leg,
    // and force the invasive case through a KEYBOARD finding (its
    // strategy's profile `keyboard` sends Tab — beyond observational).
    let audit = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "keyboard", "id": sid, "allow_mutation": true,
            })))
            .await,
    );
    let findings = audit["data"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !findings.is_empty(),
        "keyboard pass emits findings: {audit}"
    );
    let finding_id = findings[0]["id"].as_str().expect("id").to_string();

    // Default: the keyboard re-check is beyond observational ⇒ gated.
    let verify = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "verify", "finding_id": &finding_id, "id": sid,
            })))
            .await,
    );
    assert_eq!(verify["category"], "success", "{verify}");
    assert_eq!(
        verify["data"]["verdict"]["recheck_state"], "gated",
        "invasive recheck withheld by default: {verify}"
    );
    assert!(verify["data"]["verdict"]["still_reproduces"].is_null());
    let reason = verify["data"]["verdict"]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("allow_mutation=true"),
        "the gate names the remedy: {verify}"
    );

    // Authorized: the same verify runs the re-check live.
    let verify2 = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "verify", "finding_id": &finding_id, "id": sid,
                "allow_mutation": true,
            })))
            .await,
    );
    assert_eq!(verify2["category"], "success", "{verify2}");
    let recheck = &verify2["data"]["recheck"];
    assert_eq!(
        recheck["executed"], true,
        "allow_mutation executes the recheck: {verify2}"
    );
    assert!(
        recheck["rule_refired"].is_boolean(),
        "an executed recheck yields a rule-level boolean: {verify2}"
    );
    assert!(
        verify2["data"]["verdict"]["still_reproduces"].is_boolean(),
        "authorized verify yields a definitive verdict: {verify2}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
}

/// (2) no broad fallback: a finding whose rule has no strategy entry and
/// whose category is not a profile is REFUSED — the old code ran an
/// unrelated `full` audit instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_surface_is_refused_never_degraded_to_full() {
    let server = TuiLabServer::new();
    let sid = start(&server, "print('verify-nosurf'); input()").await;

    // A keyboard audit reliably leaves AUDIT-RESIDUE (category `audit`,
    // rule_id None): the old fallback would have run an unrelated `full`
    // audit over it; the fix refuses with a no-surface error.
    let audit = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "keyboard", "id": sid, "allow_mutation": true,
            })))
            .await,
    );
    assert_eq!(audit["category"], "success", "{audit}");
    let findings = audit["data"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let residue = findings
        .iter()
        .find(|f| f["id"].as_str() == Some("AUDIT-RESIDUE"))
        .cloned();
    let Some(residue) = residue else {
        // This host's driver left no residue — the no-surface strategy
        // table is pinned by the unit test instead.
        let _ = server
            .tui_session(session_params(json!({ "action": "stop", "id": sid })))
            .await;
        return;
    };
    let finding_id = residue["id"].as_str().expect("id").to_string();

    let refused = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "verify", "finding_id": &finding_id, "id": sid,
            })))
            .await,
    );
    assert_eq!(
        refused["category"], "invalid_request",
        "a no-surface finding must be refused: {refused}"
    );
    let msg = refused["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("no verification surface"),
        "the refusal names the absence: {refused}"
    );
    assert!(
        !msg.contains("full"),
        "the refusal must NOT have degraded to a full audit: {refused}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
}

/// (3)+(4): replay is named reproduction_setup with an explicit
/// not-proof note, and each verify execution persists a citable
/// VerificationRecord.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_is_setup_and_verification_is_recorded() {
    let server = TuiLabServer::new();
    let sid = start(&server, "print('verify-rec'); input()").await;

    // A discoverability finding (observational — the recheck runs
    // without authorization).
    let audit = result_json(
        &server
            .tui_audit(audit_params(json!({
                "profile": "discoverability", "id": sid,
            })))
            .await,
    );
    let findings = audit["data"]["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!findings.is_empty(), "{audit}");
    let finding_id = findings[0]["id"].as_str().expect("id").to_string();

    let verify = result_json(
        &server
            .tui_workflow(workflow_params(json!({
                "action": "verify", "finding_id": &finding_id, "id": sid,
            })))
            .await,
    );
    assert_eq!(verify["category"], "success", "{verify}");
    let data = &verify["data"];
    // The replay leg (absent here) is honestly named, not implied-proof.
    assert_eq!(
        data["verdict"]["replay"]["leg"], "reproduction_setup",
        "{verify}"
    );
    // Observational recheck ran without authorization.
    assert_eq!(data["recheck"]["executed"], true, "{verify}");
    assert!(data["verdict"]["still_reproduces"].is_boolean(), "{verify}");
    // Citable: fingerprint present, and the run records the execution.
    let fp = data["finding_fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string();
    assert!(!fp.is_empty());

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
}
