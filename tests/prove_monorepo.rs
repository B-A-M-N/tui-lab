//! "Prove the whole thing, not the pieces" — item 43.
//!
//! Monorepo reality: a workspace root holds the lockfile and the workspace
//! manifest; the TUI app lives in a member crate; npm workspaces nest a
//! package under a root manifest. From INSIDE a member, detection,
//! ProjectContext, and ProjectLocator must resolve the right member vs
//! workspace roots — and a session launched from the member dir audits
//! against the member, not the root, not the filesystem.

use std::fs;
use tempfile::TempDir;

/// Build a synthetic Cargo workspace:
///   root/  Cargo.toml (workspace.members) + Cargo.lock
///   root/crates/tui-app/Cargo.toml  (depends on ratatui)
///   root/crates/lib/Cargo.toml      (no framework — a plain member)
fn cargo_workspace() -> TempDir {
    let dir = TempDir::new().expect("workspace root");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    )
    .expect("workspace manifest");
    fs::write(dir.path().join("Cargo.lock"), "version = 4\n").expect("lockfile");
    let app = dir.path().join("crates").join("tui-app");
    fs::create_dir_all(&app).expect("app crate");
    fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = \"tui-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nratatui = \"0.29\"\n",
    )
    .expect("app manifest");
    let lib = dir.path().join("crates").join("lib");
    fs::create_dir_all(&lib).expect("lib crate");
    fs::write(
        lib.join("Cargo.toml"),
        "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("lib manifest");
    dir
}

/// Build a synthetic npm monorepo:
///   root/package.json (workspaces: ["packages/*"])
///   root/packages/tui/package.json  (depends on ink)
fn npm_workspaces() -> TempDir {
    let dir = TempDir::new().expect("npm root");
    fs::write(
        dir.path().join("package.json"),
        r#"{"name":"monorepo","private":true,"workspaces":["packages/*"]}"#,
    )
    .expect("root package.json");
    let pkg = dir.path().join("packages").join("tui");
    fs::create_dir_all(&pkg).expect("package dir");
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"tui","version":"0.1.0","dependencies":{"ink":"^5"}}"#,
    )
    .expect("package.json");
    dir
}

/// From inside the member crate: package root = the member, workspace root
/// = the lockfile ancestor; detection runs against the MEMBER (ratatui),
/// not the root; and the roots serialize distinctly.
#[test]
fn cargo_member_resolves_both_roots_and_detects_from_member() {
    let ws = cargo_workspace();
    let member = ws.path().join("crates").join("tui-app");

    let ctx = tui_lab::framework::context::ProjectContext::resolve(member.to_str().unwrap());
    assert_eq!(
        ctx.package_root.as_deref(),
        Some(member.to_str().unwrap()),
        "package root is the member crate, not the workspace"
    );
    assert_eq!(
        ctx.workspace_root.as_deref(),
        Some(ws.path().to_str().unwrap()),
        "workspace root is the lockfile ancestor"
    );
    assert_ne!(ctx.package_root, ctx.workspace_root);

    // Detection FROM the member dir names ratatui via the member's own
    // manifest — a workspace-root-only scan would have missed it.
    let det = tui_lab::framework::detect::detect(member.to_str().unwrap());
    assert_eq!(
        det.framework(),
        Some("ratatui"),
        "member manifest drives detection: {:?}",
        det.primary
    );

    // From the PLAIN member (no framework): nothing is invented.
    let plain = ws.path().join("crates").join("lib");
    let det2 = tui_lab::framework::detect::detect(plain.to_str().unwrap());
    assert!(
        det2.candidates.is_empty(),
        "a framework-less member detects nothing: {:?}",
        det2.candidates
    );
}

/// ProjectLocator from a deeply nested member path walks up to the member
/// manifest (its boundary) — and, lacking one in `crates/lib`, keeps
/// walking to the workspace root (which holds Cargo.toml).
#[test]
fn locator_walks_to_the_nearest_member_boundary() {
    let ws = cargo_workspace();
    // Nested source dir inside the framework member.
    let nested = ws.path().join("crates").join("tui-app").join("src");
    fs::create_dir_all(&nested).expect("src dir");
    let loc = tui_lab::session::ProjectLocator::locate(None, nested.to_str().unwrap());
    assert_eq!(
        loc.manifest_roots,
        vec![ws
            .path()
            .join("crates")
            .join("tui-app")
            .to_string_lossy()
            .to_string()],
        "the member manifest is the boundary, not the workspace root: {:?}",
        loc.manifest_roots
    );

    // A manifest-LESS member (shared code with no own Cargo.toml): the
    // walk continues to the workspace root, whose manifest anchors it.
    let shared_src = ws.path().join("crates").join("shared").join("src");
    fs::create_dir_all(&shared_src).expect("shared src");
    let loc2 = tui_lab::session::ProjectLocator::locate(None, shared_src.to_str().unwrap());
    assert_eq!(
        loc2.manifest_roots,
        vec![ws.path().to_string_lossy().to_string()],
        "no member manifest → the workspace root manifest wins"
    );

    // And the plain member WITH its own manifest (crates/lib) is its own
    // boundary — the nearest-manifest rule, not the workspace rule.
    let lib_src = ws.path().join("crates").join("lib").join("src");
    fs::create_dir_all(&lib_src).expect("lib src");
    let loc3 = tui_lab::session::ProjectLocator::locate(None, lib_src.to_str().unwrap());
    assert_eq!(
        loc3.manifest_roots,
        vec![ws
            .path()
            .join("crates")
            .join("lib")
            .to_string_lossy()
            .to_string()],
        "a plain member's own manifest is its boundary"
    );
}

/// npm workspaces: the member package resolves as package root; the root
/// manifest's `workspaces` key anchors the workspace; ink is detected FROM
/// the member.
#[test]
fn npm_member_resolves_roots_and_detects_from_member() {
    let ws = npm_workspaces();
    let member = ws.path().join("packages").join("tui");
    let ctx = tui_lab::framework::context::ProjectContext::resolve(member.to_str().unwrap());
    assert_eq!(ctx.package_root.as_deref(), Some(member.to_str().unwrap()));
    assert_eq!(
        ctx.workspace_root.as_deref(),
        Some(ws.path().to_str().unwrap()),
        "the root package.json's workspaces key anchors the monorepo"
    );
    assert!(
        ctx.workspace_anchor.unwrap().contains("workspaces"),
        "the anchor names the mechanism"
    );
    let det = tui_lab::framework::detect::detect(member.to_str().unwrap());
    assert_eq!(det.framework(), Some("ink"), "{:?}", det.primary);
}

/// The MCP-layer path end-to-end: `tui_framework action=detect` with
/// cwd=<member> resolves the member boundary through ProjectLocator and
/// reports both roots plus the framework — over the REAL stdio wire, the
/// way an agent actually calls it (the per-tool Rust methods are internal;
/// the wire is the contract).
#[test]
fn mcp_detect_from_member_reports_member_context() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let ws = cargo_workspace();
    let member = ws.path().join("crates").join("tui-app");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tui-lab"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let mut send = |v: serde_json::Value| {
        writeln!(stdin, "{v}").expect("write");
        stdin.flush().expect("flush");
    };
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "monorepo-test", "version": "0" },
        }
    }));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("init response");
    send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));

    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2,
        "method": "tools/call",
        "params": {
            "name": "tui_framework",
            "arguments": { "action": "detect", "cwd": member.to_string_lossy() },
        }
    }));
    loop {
        line.clear();
        let n = stdout.read_line(&mut line).expect("read response");
        assert!(n > 0, "server closed before responding");
        let v: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC line");
        if v["id"] == 2 {
            let data = &v["result"]["structuredContent"]["data"];
            assert_eq!(
                v["result"]["structuredContent"]["category"], "success",
                "detect over the wire: {v}"
            );
            assert_eq!(
                data["framework"]["primary"]["name"],
                serde_json::json!("ratatui"),
                "{v}"
            );
            let pc = &data["project_context"];
            assert_eq!(
                pc["package_root"],
                serde_json::json!(member.to_string_lossy().to_string()),
                "the MCP surface reports the member as package root: {pc}"
            );
            assert_eq!(
                pc["workspace_root"],
                serde_json::json!(ws.path().to_string_lossy().to_string()),
                "{pc}"
            );
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The session side: launched from the member dir, the fixture's audits
/// and observations carry the member-relative provenance. The launch cwd
/// rides on the session's LaunchSpec, so restart-replay reproduces the
/// same member-relative launch rather than degrading to the server cwd.
#[tokio::test]
async fn session_from_member_dir_keeps_launch_cwd_provenance() {
    let ws = cargo_workspace();
    let member = ws.path().join("crates").join("tui-app");
    // A tiny "app" inside the member: the member dir is the launch cwd.
    fs::write(
        member.join("app.py"),
        "import sys, time\nprint('MEMBER-APP')\nsys.stdout.flush()\ninput()\ntime.sleep(30)\n",
    )
    .expect("member app");
    let pool = tui_lab::session::SessionPool::new();
    let sid = pool
        .start(
            "python3",
            &["app.py".to_string()],
            Some(member.to_str().unwrap()),
            &[],
            80,
            24,
            "auto",
            "local",
        )
        .await
        .expect("launch from member dir");
    {
        let member_path = member.to_str().unwrap().to_string();
        let (cwd, out) = pool
            .with_session(Some(&sid), move |sess| {
                sess.observe(300).expect("first frame");
                let spec = sess.launch().expect("launched sessions carry their spec");
                let cwd = spec.cwd.clone();
                // The member app actually runs.
                let out = sess
                    .wait(tui_lab::backend::WaitCond::Text("MEMBER-APP".into()), 5_000)
                    .expect("wait");
                (cwd, out)
            })
            .await
            .expect("launch provenance job");
        assert_eq!(
            cwd.as_deref(),
            Some(member_path.as_str()),
            "the launch cwd is the member dir — restart-replay reproduces it"
        );
        assert!(
            out.met,
            "member app must run: {:?}",
            out.state.viewport_text
        );
    }
    // Restart reproduces the member-relative launch.
    pool.restart(&sid).await.expect("restart");
    let restarted = pool
        .with_session(Some(&sid), |sess| {
            sess.wait(tui_lab::backend::WaitCond::Text("MEMBER-APP".into()), 5_000)
                .expect("wait after restart")
                .met
        })
        .await
        .expect("restart wait job");
    assert!(restarted, "restarted member app must run again");
    pool.stop(&sid).await.ok();
}

/// The workspace root itself is NOT a package: ProjectContext from the
/// root reports package_root = the root (its workspace manifest is also a
/// Cargo.toml) but honest anchors — and detection there names nothing
/// (workspace manifests carry no framework deps).
#[test]
fn workspace_root_is_not_mistaken_for_a_framework_package() {
    let ws = cargo_workspace();
    let ctx = tui_lab::framework::context::ProjectContext::resolve(ws.path().to_str().unwrap());
    assert_eq!(
        ctx.package_root.as_deref(),
        Some(ws.path().to_str().unwrap()),
        "the root manifest anchors the package walk at the root itself"
    );
    assert_eq!(
        ctx.workspace_root.as_deref(),
        Some(ws.path().to_str().unwrap())
    );
    let det = tui_lab::framework::detect::detect(ws.path().to_str().unwrap());
    assert!(
        det.candidates.is_empty(),
        "a workspace manifest has no framework deps — nothing invented: {:?}",
        det.candidates
    );
}

/// Premise guard: the synthetic lockfile is anchored where the resolver
/// expects it (it reads the file, not the registry).
#[test]
fn synthetic_lockfile_is_anchored() {
    let ws = cargo_workspace();
    assert!(ws.path().join("Cargo.lock").is_file());
}
