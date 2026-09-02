//! "Prove the whole thing, not the pieces" — item 38.
//!
//! The doctor's native-cooperation tier must verify COOPERATION
//! (frames land, the declared tree merges over inference), not
//! configuration (an env var was exported). The probe is shared library
//! code (`diagnostic::native_cooperation_probe`), so these tests exercise
//! exactly what `hermes-tui-lab doctor` runs:
//!
//! * the happy path against the shipped cooperative fixture;
//! * the honest-failure paths (a silent channel is NOT cooperation);
//! * the doctor binary itself, end-to-end, including the exit-code gate.

use std::process::Command;

/// The probe launches the real fixture through a real session and proves
/// all three facts: frames land, the declared control resolves in FUSED
/// semantics with native provenance, and the app's focus declaration
/// overrides inference.
#[test]
fn doctor_native_probe_proves_cooperation_end_to_end() {
    let r = tui_lab::diagnostic::native_cooperation_probe();
    assert!(
        r.ran,
        "the environment has python3; the probe must run (detail: {})",
        r.detail
    );
    assert!(
        r.frames_received > 0,
        "the fixture must have written frames: {r:?}"
    );
    assert_eq!(
        r.frames_invalid, 0,
        "a healthy fixture writes zero invalid frames: {r:?}"
    );
    assert!(
        r.native_control_resolved,
        "the declared Save control must resolve with native provenance in fused semantics: {r:?}"
    );
    assert!(
        r.native_focus_applied,
        "the app's focus declaration must override inference: {r:?}"
    );
}

/// The claim is about the WHOLE path, so the probe must be stronger than
/// the old env-var check it replaces: a non-cooperative app (never reads
/// the variable) must produce a ran=true, frames=0 verdict with a detail
/// that names silence. Proven with a real non-cooperative child (the same
/// shape every ordinary app produces).
#[test]
fn probe_distinguishes_silence_from_cooperation() {
    let mut sess = tui_lab::session::state::Session::new("probe-silent".into(), "python3".into());
    sess.start_with_spec(tui_lab::session::state::LaunchSpec {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            "import time; print('plain app'); time.sleep(5)".into(),
        ],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
        backend: "auto".into(),
        isolation: "local".into(),
    })
    .expect("spawn");
    let _ = sess.observe(200);
    sess.poll_native();
    let st = sess.adapter_status();
    assert!(st.adapter_available, "harness exported the env var");
    assert_eq!(
        st.frames_received, 0,
        "a non-cooperative app produces zero frames — the exact case the old env-var check could not distinguish"
    );
    assert!(!st.native_channel_active);
    sess.stop().ok();
}

/// The doctor binary runs the probe as its own tier: the line appears,
/// reports the fixture's cooperation, and the process exits 0 when core
/// subsystems (now including native cooperation) are operational.
#[test]
fn doctor_binary_reports_native_cooperation_and_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_hermes-tui-lab");
    let out = Command::new(bin)
        .arg("doctor")
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "doctor must exit 0 on a healthy system; stdout:\n{stdout}"
    );
    let line = stdout
        .lines()
        .find(|l| l.contains("Native cooperation"))
        .expect("doctor must carry the native cooperation tier");
    assert!(
        line.contains("[ok]"),
        "on this host the fixture cooperates — the tier must be [ok]: {line}"
    );
    assert!(
        line.contains("fixture declared its tree"),
        "the detail must describe the proven cooperation, not configuration: {line}"
    );
}

/// The tier degrades honestly when python3 is absent: the doctor's other
/// python-dependent probes coexisted with a WARN for missing python3, and
/// the cooperation tier must follow the same rule — skipped (warn), never
/// a fake pass, never a fail for an absent optional runtime. Proven by
/// running doctor with PATH stripped of python3 (a scratch bin dir holding
/// only what the other probes need is unnecessary: the PTY probe catches
/// its own failure).
#[test]
fn doctor_without_python3_skips_the_tier_with_a_warn() {
    let bin = env!("CARGO_BIN_EXE_hermes-tui-lab");
    // A PATH that almost certainly lacks python3: an empty scratch dir.
    let scratch = tempfile::tempdir().expect("scratch");
    let out = Command::new(bin)
        .arg("doctor")
        .env("PATH", scratch.path())
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let line = stdout
        .lines()
        .find(|l| l.contains("Native cooperation"))
        .expect("the tier must still be reported");
    assert!(
        line.contains("[warn]") && line.contains("skipped"),
        "a missing python3 must degrade to warn/skip, never fail or fake-pass: {line}"
    );
    // The core gate is unaffected by the skip when nothing else could run
    // either; what matters here is the tier's honesty, already asserted.
}
