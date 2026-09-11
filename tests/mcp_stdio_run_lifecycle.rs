mod support;
use support::mcp::{initialize, McpProc};

#[test]
fn stdio_e2e_run_lifecycle_provenance() {
    let mut mcp = McpProc::spawn();

    initialize(&mut mcp, "tui-lab-e2e-provenance");
    // ── Scenario (a): persistent run A, dirty state → new → A flushed ──
    let persist_root =
        std::env::temp_dir().join(format!("tui-lab-e2e-provrun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&persist_root);
    std::fs::create_dir_all(&persist_root).expect("persist root");

    let start = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('prov-ready'); input()"],
            "cwd": persist_root.to_string_lossy(),
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start["category"], "success", "start: {start}");
    let sess_a = start["data"]["session"].as_str().unwrap().to_string();

    // Dirty state: drive the session so run A holds in-memory evidence.
    let act = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "type", "text": "a-marker", "id": sess_a }),
    );
    assert_eq!(act["category"], "success", "act into A: {act}");

    // Promote A to persistent so flush-before-new is meaningful.
    let persist = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "persist", "root": persist_root.to_string_lossy() }),
    );
    assert_eq!(persist["category"], "success", "persist A: {persist}");
    let run_a = persist["data"]["run_id"].as_str().unwrap().to_string();
    let artifact_a = persist["data"]["artifact_root"]
        .as_str()
        .unwrap()
        .to_string();

    // Now begin a fresh run. The old persistent run A must be flushed, and
    // the response must NAME that flush — never silently abandon it.
    let new_run = mcp.tool("tui_run", serde_json::json!({ "action": "new" }));
    assert_eq!(new_run["category"], "success", "new: {new_run}");
    assert_eq!(
        new_run["data"]["previous_run"],
        run_a.as_str(),
        "new names the run it left: {new_run}"
    );
    assert_eq!(
        new_run["data"]["previous_flushed"], true,
        "persistent run A must be flushed on new: {new_run}"
    );
    let run_b = new_run["data"]["new_run_id"].as_str().unwrap().to_string();
    assert_ne!(run_a, run_b, "new run must have a fresh identity");
    // The flush is real, not a claimed flag: run A's manifest on disk is
    // written and still identifies the same run (the durable side caught up
    // to the in-memory evidence instead of dropping it).
    let manifest_a: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::path::Path::new(&artifact_a).join("run.json"))
            .expect("run A manifest on disk"),
    )
    .expect("run A manifest parses as JSON");
    assert_eq!(
        manifest_a["run_id"],
        run_a.as_str(),
        "flush after new must leave run A's durable manifest: {manifest_a}"
    );

    // ── Scenario (b): A's session is foreign; close B kill_sessions → A's
    // session survives. ──
    let close_b = mcp.tool(
        "tui_run",
        serde_json::json!({ "action": "close", "kill_sessions": true }),
    );
    assert_eq!(close_b["category"], "success", "close B: {close_b}");
    assert_eq!(close_b["data"]["closed"], true);
    assert!(
        close_b["data"]["owned_sessions"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "run B owns no sessions after new: {close_b}"
    );
    assert!(
        close_b["data"]["foreign_live_sessions"]
            .as_array()
            .map(|a| a.iter().any(|s| s.as_str() == Some(sess_a.as_str())))
            .unwrap_or(false),
        "close B must see A's session as foreign: {close_b}"
    );
    assert!(
        close_b["data"]["sessions_stopped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "kill_sessions on B must NEVER touch A's foreign session: {close_b}"
    );
    // Positive proof the foreign child process SURVIVED B's kill: resume
    // run A (the session's home run, on disk) and drive sess_a again. If B's
    // kill had reaped the child, resume-A's drive would fail for a dead
    // process. This also proves the provenance invariant in the other
    // direction: resume re-binds the foreign session to its originating run,
    // so its traffic lands in the right bundle again.
    let resume_a = mcp.tool(
        "tui_run",
        serde_json::json!({
            "action": "resume",
            "run_dir": artifact_a,
        }),
    );
    assert_eq!(resume_a["category"], "success", "resume A: {resume_a}");
    let act_foreign = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "x", "id": sess_a }),
    );
    assert_eq!(
        act_foreign["category"], "success",
        "A's session must survive B's kill close and drive under its home run: {act_foreign}"
    );
    // Leave run A durable (resumed session belongs to it) and open a fresh
    // ephemeral current run so scenario (c) has an OPEN current run whose
    // sessions must survive a corrupt-resume refusal. sess_a becomes foreign
    // again and is simply cleaned up at server teardown.
    let open_b = mcp.tool("tui_run", serde_json::json!({ "action": "new" }));
    assert_eq!(
        open_b["category"], "success",
        "re-open after resume A: {open_b}"
    );

    // ── Scenario (c): resume a CORRUPT run with detach=true → refuse at
    // restore (step 1), current sessions survive. ──
    // Make an open-run session in the CURRENT run that had better survive a
    // failed resume.
    let start_c = mcp.tool(
        "tui_session",
        serde_json::json!({
            "action": "start",
            "command": "python3",
            "args": ["-c", "print('prov-c-ready'); input()"],
            "cwd": persist_root.to_string_lossy(),
            "cols": 80, "rows": 24,
        }),
    );
    assert_eq!(start_c["category"], "success", "start C: {start_c}");
    let sess_c = start_c["data"]["session"].as_str().unwrap().to_string();

    // A corrupt run: a directory named like a persisted run whose run.json
    // is NOT parseable JSON. restore() (manifest::load) errors, which is the
    // step-1 refusal — before the flush and long before any detach.
    let corrupt_dir = persist_root.join("run-corrupt");
    std::fs::create_dir_all(&corrupt_dir).expect("corrupt dir");
    std::fs::write(corrupt_dir.join("run.json"), "{ this is: not json !!!")
        .expect("write corrupt manifest");

    let resume_c = mcp.tool(
        "tui_run",
        serde_json::json!({
            "action": "resume",
            "run_dir": corrupt_dir.to_string_lossy(),
            "detach_existing_sessions": true,
        }),
    );
    assert_eq!(
        resume_c["category"], "invalid_request",
        "corrupt resume must refuse, even with detach=true: {resume_c}"
    );
    assert!(
        resume_c["error"]
            .as_str()
            .map(|m| m.contains("cannot restore"))
            .unwrap_or(false),
        "error names the restore failure: {resume_c}"
    );
    // The current run is untouched, so its session is still alive AND still
    // owned by the open run: driving still works.
    let act_c = mcp.tool(
        "tui_act",
        serde_json::json!({ "action": "key", "key": "y", "id": sess_c }),
    );
    assert_eq!(
        act_c["category"], "success",
        "corrupt-resume refusal must leave the current session driving: {act_c}"
    );

    // ── Scenario (d): closed run → restart session → refused BEFORE the
    // process is mutated. ──
    // Close the current run without killing sessions: sess_c stays live.
    let close_c = mcp.tool("tui_run", serde_json::json!({ "action": "close" }));
    assert_eq!(close_c["category"], "success", "close: {close_c}");
    assert_eq!(close_c["data"]["closed"], true);
    assert!(
        close_c["data"]["sessions_stopped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "close without kill keeps the session live: {close_c}"
    );
    // Restart is destructive (it kills the child). On a closed run it must be
    // refused with run_closed, and the refusal must happen BEFORE the child
    // is torn down — proven because sess_c is still the same live session.
    let restart_c = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "restart", "id": sess_c }),
    );
    assert_eq!(
        restart_c["category"], "run_closed",
        "closed run must refuse restart: {restart_c}"
    );
    assert!(
        restart_c["error"]
            .as_str()
            .map(|m| m.contains("closed") && m.contains("resume"))
            .unwrap_or(false),
        "error names the state and the remedy: {restart_c}"
    );

    // The child was never mutated by the refused restart: while driving
    // on a closed run is gated (with_sess refuses run_closed for observe),
    // `stop` bypasses that gate and tears down the REAL live process. Its
    // success here proves the restart refusal preceded any process mutation —
    // restart is the destructive kill path, and it never ran.
    let stop_c = mcp.tool(
        "tui_session",
        serde_json::json!({ "action": "stop", "id": sess_c }),
    );
    assert_eq!(
        stop_c["category"], "success",
        "stop succeeds on the same live process the refused restart never touched: {stop_c}"
    );

    let _ = std::fs::remove_dir_all(&persist_root);
}
