//! Beta-audit P0.7: ONE mutation-authorization model for contract
//! conformance.
//!
//! The defect: `tui_audit profile=contract` went through SafetyPolicy
//! (safe-only default), but `tui_contract action=status/baseline/compare`
//! invoked the conformance engine directly — sending declared keys,
//! Escape/Tab probes, and resizes from an operation named `status`,
//! bypassing the mutation consent the audit surface required.
//!
//! The invariant under test: the conformance engine runs PASSIVE by
//! default (driving groups reported Unverified, nothing sent), and only
//! an explicit `allow_mutation=true` executes the driving groups.

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

fn contract_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiContractParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn session_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiSessionParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

/// A counter the fixture writes to a file on every key it receives: the
/// direct observable for "did the conformance engine send anything".
/// Tab is expected (the fixture echoes keys), so a driving check bumps
/// the counter; a passive check must leave it at zero.
fn counter_fixture(marker: &str) -> String {
    format!(
        r#"
import sys, os, tty
tty.setraw(0)
path = {marker:?}
count = 0
if os.path.exists(path):
    count = int(open(path).read() or 0)
print("READY")
sys.stdout.flush()
while True:
    ch = sys.stdin.buffer.read(1)
    if ch == b'\x1b':
        break
    if ch and ch != b'\x00':
        count += 1
        open(path, "w").write(str(count))
"#
    )
}

async fn start_counter(server: &TuiLabServer, base: &std::path::Path) -> (String, String) {
    let marker = base.join("keys").to_string_lossy().to_string();
    std::fs::write(&marker, "0").expect("seed counter");
    let out = server
        .tui_session(session_params(json!({
            "action": "start", "command": "python3",
            "args": ["-c", counter_fixture(&marker)],
            "cols": 80, "rows": 24,
        })))
        .await;
    let sid = result_json(&out)["data"]["session"]
        .as_str()
        .expect("session id")
        .to_string();
    (sid, marker)
}

/// A loaded contract whose interactions would drive the app if the
/// engine ran them.
fn write_driving_contract(base: &std::path::Path, name: &str) -> String {
    let path = base.join(name);
    std::fs::write(
        &path,
        "schema:\n  name: driving-fixture\n  version: \"1\"\ninteractions:\n  - name: press-x\n    keys: [\"x\"]\n    expect: []\n",
    )
    .expect("write contract");
    path.to_string_lossy().to_string()
}

/// The core P0.7 guarantee: `status` (and `baseline`, and `compare`) do
/// NOT drive the app by default. The interaction is declared but the
/// key counter must still read zero afterward, and the interaction
/// check must be Unverified — an honest absence, not a pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passive_by_default_status_never_drives() {
    let server = TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-p07p-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");
    let (sid, counter) = start_counter(&server, &base).await;
    let contract_path = write_driving_contract(&base, "passive.yaml");

    let loaded = server
        .tui_contract(contract_params(json!({
            "action": "load", "path": contract_path, "id": sid,
        })))
        .await;
    assert!(
        !loaded.is_error.unwrap_or(false),
        "load: {}",
        result_text(&loaded)
    );

    let body = result_json(
        &server
            .tui_contract(contract_params(json!({
                "action": "status", "id": sid,
            })))
            .await,
    );
    assert_eq!(body["category"], "success", "{body}");
    let status = &body["data"];
    assert_eq!(
        status["exec_policy"], "passive",
        "default status is passive: {status}"
    );
    let interaction = status["results"]
        .as_array()
        .expect("results array")
        .iter()
        .find(|r| r["group"] == "interaction" && r["name"] == "press-x")
        .expect("declared interaction appears in results")
        .clone();
    assert_eq!(
        interaction["verdict"], "unverified",
        "a driving check is Unverified under passive, never silently run: {interaction}"
    );
    assert!(
        interaction["detail"]
            .as_str()
            .unwrap_or("")
            .contains("allow_mutation"),
        "the result names how to allow it: {interaction}"
    );

    // The app received NOTHING.
    let sent: u32 = std::fs::read_to_string(&counter)
        .expect("counter file")
        .trim()
        .parse()
        .expect("counter value");
    assert_eq!(sent, 0, "passive status must not drive the app");

    // The engine returned data (document/component checks ran).
    assert!(
        status["results"]
            .as_array()
            .map(|r| !r.is_empty())
            .unwrap_or(false),
        "passive still runs the static groups: {status}"
    );
    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

/// Explicit consent flips the policy: the same check with
/// allow_mutation=true executes the declared interaction (the counter
/// moves), and the exec_policy is reported as driving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_mutation_executes_the_driving_groups() {
    let server = TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-p07d-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");
    let (sid, counter) = start_counter(&server, &base).await;
    let contract_path = write_driving_contract(&base, "driving.yaml");

    let loaded = server
        .tui_contract(contract_params(json!({
            "action": "load", "path": contract_path, "id": sid,
        })))
        .await;
    assert!(
        !loaded.is_error.unwrap_or(false),
        "load: {}",
        result_text(&loaded)
    );

    let body = result_json(
        &server
            .tui_contract(contract_params(json!({
                "action": "status", "id": sid, "allow_mutation": true,
            })))
            .await,
    );
    let status = &body["data"];
    assert_eq!(
        status["exec_policy"], "driving",
        "consent flips the policy: {body}"
    );
    let raw = std::fs::read_to_string(&counter).expect("counter file");
    let sent: u32 = raw.trim().parse().expect("counter value");
    assert!(
        sent > 0,
        "the declared interaction was actually executed under consent (counter file = {raw:?}): {status}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}

/// `baseline` and `compare` are also gated: passive by default (the
/// baseline records honest Unverified results), driving only under
/// explicit consent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_and_compare_are_gated_too() {
    let server = TuiLabServer::new();
    let base = std::env::temp_dir().join(format!("tui-lab-p07b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("base");
    let (sid, counter) = start_counter(&server, &base).await;
    let contract_path = write_driving_contract(&base, "gated.yaml");

    let loaded = server
        .tui_contract(contract_params(json!({
            "action": "load", "path": contract_path, "id": sid,
        })))
        .await;
    assert!(!loaded.is_error.unwrap_or(false));

    let baseline = &result_json(
        &server
            .tui_contract(contract_params(json!({
                "action": "baseline", "id": sid, "label": "snap",
            })))
            .await,
    )["data"];
    assert_eq!(baseline["verdict"], "UNVERIFIED", "{baseline}");

    // Compare passively: no regression, nothing driven.
    let compare = &result_json(
        &server
            .tui_contract(contract_params(json!({
                "action": "compare", "id": sid, "baseline": "snap",
            })))
            .await,
    )["data"];
    let sent_after_passive: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("value");
    assert_eq!(
        sent_after_passive, 0,
        "baseline+compare passive must not drive: {compare}"
    );
    assert!(
        compare["regressions"]
            .as_array()
            .map(|r| r.is_empty())
            .unwrap_or(false),
        "unverified baselines do not regress: {compare}"
    );

    // Driving compare under consent: the key lands now.
    let _ = server
        .tui_contract(contract_params(json!({
            "action": "compare", "id": sid, "baseline": "snap", "allow_mutation": true,
        })))
        .await;
    let sent_after_driving: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("value");
    assert!(
        sent_after_driving > 0,
        "driving compare executed the interaction under consent"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid })))
        .await;
    let _ = std::fs::remove_dir_all(&base);
}
