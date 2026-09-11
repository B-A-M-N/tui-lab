//! Audit finding 38 live conformance: attach through TmuxBackend's
//! persistent `tmux -C` client, force pane output, and verify the retained
//! control ring carries exact `%output` bytes. The sandbox contract for
//! optional tmux integration is honest skip when its server/socket cannot
//! be created.

use std::time::Duration;
use tui_lab::backend::tmux::TmuxBackend;
use tui_lab::backend::{TerminalBackend, WaitCond};

fn tmux_available() -> bool {
    // A successful `has-session` requires both the binary and a usable
    // server socket; an EPERM or missing server is an environment skip, not
    // a product failure.
    std::process::Command::new("tmux")
        .args(["has-session"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn tmux_control_mode_retains_output_bytes_and_lifecycle() {
    if !tmux_available() {
        eprintln!("SKIP: usable tmux server/socket unavailable; control-mode E2E requires a socket-permitted environment");
        return;
    }
    let sess = format!("tuilab-control-{}", std::process::id());
    let created = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &sess,
            "-x",
            "80",
            "-y",
            "24",
            "python3",
            "-u",
            "-c",
            "print('CONTROL-READY', flush=True); import time; time.sleep(20)",
        ])
        .output()
        .expect("spawn tmux session");
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut backend = TmuxBackend::attach(&format!("{sess}:0.0"), 80, 24)
        .expect("attach through control backend");
    backend
        .start("", &[], None, &[], 80, 24)
        .expect("verify pane");
    let screen = backend
        .wait(
            WaitCond::Text("CONTROL-READY".into()),
            Duration::from_secs(5),
        )
        .expect("wait");
    assert!(screen.met, "control attach must observe pane output");

    let raw = backend.recent_raw_output().expect("control raw output");
    assert!(
        raw.windows(b"CONTROL-READY".len())
            .any(|w| w == b"CONTROL-READY"),
        "control ring must retain exact %output bytes: {:?}",
        String::from_utf8_lossy(&raw)
    );
    let (capacity, dropped) = backend.raw_output_stats();
    assert!(capacity > 0);
    assert_eq!(dropped, 0);
    let events = backend.tmux_control_events(0);
    assert!(
        events.iter().any(|e| matches!(
            e.kind,
            tui_lab::backend::tmux::TmuxControlEventKind::Output { .. }
        )),
        "control event stream must retain output events"
    );
    let fidelity = backend
        .capabilities()
        .observability_fidelity
        .expect("fidelity");
    assert_eq!(fidelity.mode, "control_mode_ring");
    assert_eq!(fidelity.sampling_ms, 0);

    backend.stop().expect("detach");
    std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sess])
        .output()
        .ok();
}
