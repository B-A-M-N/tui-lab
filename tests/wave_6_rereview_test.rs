//! Re-review Wave 6: greenfield/test authority.
//!
//! * item 30 — contract conformance residue (session_mutated) — lives in
//!   `wave_e_test.rs` (it reuses the modal fixture);
//! * item 31 — explicit contract baselines — `contract_baseline` below;
//! * item 32 — typed contract mode ("strcit" → invalid_request);
//! * item 33 — framework candidate ranking — `framework_detect.rs`;
//! * item 34 — ProjectContext package/workspace roots — `context` module
//!   unit tests;
//! * item 35 — adapter available vs active — `adapter_status_reported`
//!   below;
//! * item 36 — unified role model — `role_model_unified` below;
//! * item 37 — structural/interaction cache split — unit tests in
//!   `semantic::cache`.

use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;
use tui_lab::session::SessionManager;

fn python_session(mgr: &mut SessionManager, code: &str) -> String {
    let args: Vec<String> = vec!["-c".to_string(), code.to_string()];
    mgr.start("python3", &args, None, &[], 80, 24, "auto", "local")
        .expect("start python session")
}

fn write_project(dir: &TempDir, files: &HashMap<&str, &str>) {
    for (name, content) in files {
        let path = dir.path().join(name);
        fs::write(path, content).unwrap();
    }
}

// ── Item 35: adapter available vs channel active ─────────────────────────

/// A plain python session never reads TUI_LAB_SEMANTIC: the adapter is
/// AVAILABLE (the harness injected the env) but the channel is NOT active
/// (zero frames) — and the serialized status carries both facts plus the
/// healthy=false verdict, so an agent can tell "harness gap" from "app
/// doesn't cooperate".
#[test]
fn adapter_status_reported_available_not_active() {
    let mut mgr = SessionManager::new();
    let sid = python_session(&mut mgr, "import time\ntime.sleep(30)");
    let sess = mgr.resolve_mut(Some(&sid)).unwrap();
    sess.observe(200).expect("baseline");

    let st = sess.adapter_status();
    assert!(
        st.adapter_available,
        "the harness injected TUI_LAB_SEMANTIC: available"
    );
    assert!(
        !st.native_channel_active,
        "a non-cooperative app never activates the channel"
    );
    assert_eq!(st.frames_received, 0);
    assert!(!st.healthy, "zero cooperation is not health");

    // The wire shape: both facts serialize (never one flattened bool).
    let v = serde_json::to_value(&st).unwrap();
    assert_eq!(v["adapter_available"], serde_json::json!(true));
    assert_eq!(v["native_channel_active"], serde_json::json!(false));
    assert_eq!(v["frames_received"], serde_json::json!(0));
    assert_eq!(v["frames_invalid"], serde_json::json!(0));
    assert_eq!(v["healthy"], serde_json::json!(false));
}

// ── Items 31/32/36 live-path helpers; the rest are unit-level. ───────────

// ── Item 32: the mode selector is typed — wire shape check ───────────────

/// The typed ContractModeParam accepts exactly the three modes; the
/// EnumVariants surface (what invalid_request names) matches.
#[test]
fn contract_mode_param_is_closed() {
    use tui_lab::mcp::params::{ContractModeParam, EnumVariants};
    assert_eq!(
        <ContractModeParam as EnumVariants>::VARIANTS,
        &["advisory", "validation", "strict"]
    );
    // Round-trip through the FromStr the Known wrapper uses.
    for v in <ContractModeParam as EnumVariants>::VARIANTS {
        assert!(v.parse::<ContractModeParam>().is_ok());
    }
    assert!("strcit".parse::<ContractModeParam>().is_err());
}

// ── Item 33: candidate ranking shapes (MCP-adjacent serialization) ───────

/// The detection JSON serializes the ranked shape: primary, candidates,
/// terminal_io, styling — a ratatui+crossterm tree names both.
#[test]
fn detection_serializes_ranked_candidates() {
    let dir = TempDir::new().unwrap();
    let mut files = HashMap::new();
    files.insert(
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nratatui = \"0.26\"\ncrossterm = \"0.27\"\n",
    );
    write_project(&dir, &files);

    let det = tui_lab::framework::detect::detect(dir.path().to_str().unwrap());
    let v = serde_json::to_value(&det).unwrap();
    assert_eq!(v["primary"]["name"], serde_json::json!("ratatui"));
    assert_eq!(v["primary"]["class"], serde_json::json!("framework"));
    assert_eq!(v["terminal_io"], serde_json::json!("crossterm"));
    assert!(v["candidates"].as_array().unwrap().len() >= 2);
}

// ── Item 34: ProjectContext rides on detect via the MCP layer shape ──────

/// A nested member crate resolves both roots and they serialize distinctly.
#[test]
fn project_context_resolves_member_and_workspace() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.lock"), "").unwrap();
    let member = dir.path().join("crates").join("app");
    fs::create_dir_all(&member).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname=\"app\"\n[dependencies]\nratatui=\"0.26\"\n",
    )
    .unwrap();

    let ctx = tui_lab::framework::context::ProjectContext::resolve(&member.to_string_lossy());
    let v = serde_json::to_value(&ctx).unwrap();
    assert_eq!(
        v["package_root"], serde_json::json!(member.to_str().unwrap()),
        "package root is the member crate"
    );
    assert_eq!(
        v["workspace_root"], serde_json::json!(dir.path().to_str().unwrap()),
        "workspace root is the lockfile ancestor"
    );
    assert_eq!(v["workspace_anchor"], serde_json::json!("Cargo.lock"));
}
