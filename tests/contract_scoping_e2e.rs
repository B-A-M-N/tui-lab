//! Beta-audit P1.5: contract scoping for multi-session runs.
//!
//! The invariant: a run represents ONE TUI project. Loading a contract
//! applies its normalization policy ONLY to sessions whose launch cwd
//! resolves to the same project root — a foreign-root session (a
//! different application sharing the run) is never silently normalized
//! by another app's policy. Foreign sessions are listed in the load
//! response, not hidden.

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

fn contract_params(v: serde_json::Value) -> Parameters<tui_lab::mcp::params::TuiContractParams> {
    Parameters(serde_json::from_value(v).unwrap())
}

fn write_contract(base: &std::path::Path, name: &str) -> String {
    let path = base.join(name);
    std::fs::write(
        &path,
        "schema:\n  name: scoped-fixture\n  version: \"1\"\ncomponents:\n  - name: save\n    role: button\n    required: true\n",
    )
    .expect("write contract");
    path.to_string_lossy().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contract_policy_scopes_to_same_project_root() {
    let server = TuiLabServer::new();
    let tag = std::process::id();
    let project_a = std::env::temp_dir().join(format!("tui-lab-p15a-{tag}"));
    let project_b = std::env::temp_dir().join(format!("tui-lab-p15b-{tag}"));
    for d in [&project_a, &project_b] {
        let _ = std::fs::remove_dir_all(d);
        // Distinct project roots (each carries its own manifest marker).
        std::fs::create_dir_all(d).expect("project dir");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
    }

    let launch = |dir: &std::path::Path| {
        json!({
            "action": "start", "command": "python3",
            "args": ["-c", "print('READY'); input()"],
            "cwd": dir.to_string_lossy(), "cols": 80, "rows": 24,
        })
    };
    let sa = result_json(&server.tui_session(session_params(launch(&project_a))).await);
    assert_eq!(sa["category"], "success", "{sa}");
    let sid_a = sa["data"]["session"].as_str().unwrap().to_string();
    let sb = result_json(&server.tui_session(session_params(launch(&project_b))).await);
    let sid_b = sb["data"]["session"].as_str().unwrap().to_string();

    // Load a contract anchored to project A (its own manifest dir).
    let contract_path = write_contract(&project_a, "scoped.yaml");
    let loaded = result_json(
        &server
            .tui_contract(contract_params(json!({
                "action": "load", "path": contract_path,
            })))
            .await,
    );
    assert_eq!(loaded["category"], "success", "{loaded}");
    let contract = &loaded["data"]["contract"];
    let applied = contract["policy_applied_to_sessions"]
        .as_array()
        .expect("applied list");
    let skipped = contract["policy_skipped_foreign_sessions"]
        .as_array()
        .expect("skipped list");
    assert!(
        applied.iter().any(|s| s.as_str() == Some(sid_a.as_str())),
        "same-project session A receives the policy: {loaded}"
    );
    assert!(
        !applied.iter().any(|s| s.as_str() == Some(sid_b.as_str())),
        "foreign-root session B must NOT receive the policy: {loaded}"
    );
    assert!(
        skipped.iter().any(|s| s.as_str() == Some(sid_b.as_str())),
        "session B is listed as skipped-foreign, not hidden: {loaded}"
    );
    assert!(
        contract["scoping_note"]
            .as_str()
            .unwrap_or_default()
            .contains("foreign-root"),
        "the note names why: {loaded}"
    );

    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid_a })))
        .await;
    let _ = server
        .tui_session(session_params(json!({ "action": "stop", "id": sid_b })))
        .await;
    let _ = std::fs::remove_dir_all(&project_a);
    let _ = std::fs::remove_dir_all(&project_b);
}
