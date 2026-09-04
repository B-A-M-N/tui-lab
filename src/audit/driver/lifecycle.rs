//! Lifecycle family: session lifecycle discipline (item 30) and
//! lifecycle-exit consuming audits.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::session::state::Session;
use serde_json::json;

use super::shared::{decode_raw, ev_other};
use crate::audit::Finding;

/// Item 29 — lifecycle/restoration audit. What state survives a restart,
/// and what state the app leaves dangling when it dies: alternate screen,
/// mouse modes, cursor visibility, and bracketed paste held at exit. This
/// is the "the app crashed and took my terminal with it" audit.
///
/// It inspects the CURRENT generation's mode state honestly (the folded
/// DECSET/DECRST timeline is the app's own traffic) and reports dangling
/// modes as findings; it does not itself restart the app (the orchestrator
/// composes this with restart-replay where risk allows).
pub fn lifecycle_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    let proc_state = session.process();
    let alive = proc_state.running;

    // Dangling terminal state only matters while the app lives; at exit the
    // engine's own teardown restores the host terminal, so these become
    // info instead of warn.
    let (sev_dangling, note_lifecycle) = if alive {
        ("warn", "app is running")
    } else {
        ("info", "app has exited (engine teardown restored the host)")
    };

    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            id: "LC-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "engine retains no raw output; mode-restoration evidence unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };

    // Fold per-mode: (ever_used, currently_set).
    let mut fold: std::collections::BTreeMap<&'static str, (bool, bool)> = Default::default();
    for m in &trace.modes {
        let e = fold.entry(m.mode).or_insert((false, false));
        e.0 = true;
        e.1 = m.set;
    }

    // Modes an app should return to the terminal: the mouse family,
    // alt screen, cursor visibility, bracketed paste.
    let restorable = [
        "alt_screen",
        "mouse_press_release",
        "mouse_button_motion",
        "mouse_any_motion",
        "bracketed_paste",
        "cursor_visible",
    ];
    let mut dangling: Vec<(&str, bool)> = Vec::new();
    for mode in restorable {
        let Some(&(used, set)) = fold.get(mode) else {
            continue;
        };
        if used && set {
            // cursor_visible SET is the healthy state; its dangling form is
            // being left OFF.
            if mode == "cursor_visible" {
                continue;
            }
            dangling.push((mode, set));
        }
        if mode == "cursor_visible" && used && !set {
            dangling.push((mode, set));
        }
    }

    if !dangling.is_empty() {
        findings.push(Finding {
            id: "LC-DANGLING".into(),
            rule_id: None,
            severity: sev_dangling.into(),
            category: "lifecycle".into(),
            summary: format!(
                "the app holds {dangling_len} terminal mode(s) it negotiated ({modes_list}) — {note_lifecycle}. If it dies without restoring them, the user's terminal is left broken (mouse reporting on, paste mangled, alt screen stuck).",
                dangling_len = dangling.len(),
                modes_list = dangling
                    .iter()
                    .map(|(m, s)| format!("{m}={}", if *s { "on" } else { "off" }))
                    .collect::<Vec<_>>()
                    .join(", "),
                note_lifecycle = note_lifecycle,
            ),
            evidence: vec![ev_other(
                "dangling_modes",
                "negotiated-but-not-restored terminal modes",
                json!({
                    "dangling": dangling.iter().map(|(m, s)| json!({ "mode": m, "set": s })).collect::<Vec<_>>(),
                    "app_running": alive,
                    "window_bytes": nbytes,
                    "dropped_head_bytes": dropped,
                    "window_complete": dropped == 0,
                    "note": "a crash with modes enabled is the classic broken-terminal report — test by killing the app mid-run and checking the host",
                }),
            )],
            confidence: 0.85,
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "LC-CLEAN".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "no dangling terminal modes in the retained window — the app negotiated nothing it still holds.".into(),
            evidence: vec![ev_other(
                "mode_fold_clean",
                "no negotiated mode left engaged",
                json!({ "window_bytes": nbytes, "window_complete": dropped == 0 }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // Window honesty: if the head of the stream was dropped, the fold may
    // MISS a DECSET from before the window — say so next to any conclusion.
    if dropped > 0 {
        findings.push(Finding {
            id: "LC-WINDOW".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: format!(
                "the raw window dropped its head ({dropped} bytes of {cap} retained) — a mode set before the window may be invisible to this fold.",
                dropped = dropped,
                cap = nbytes
            ),
            evidence: vec![ev_other(
                "window_incomplete",
                "head of the byte stream not retained",
                json!({ "dropped_head_bytes": dropped, "retained_bytes": nbytes }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}

/// Item 21 — lifecycle EXIT test. The standing audit's dangling-mode
/// finding can misread a healthy full-screen app: holding alt-screen,
/// hidden cursor, and mouse reporting WHILE RUNNING is what a TUI is
/// supposed to do. What matters is teardown at exit. This probe consumes a
/// RESTARTABLE session:
///
/// 1. observe the running app's active mode set (the baseline);
/// 2. drive the normal exit (`quit`-style action supplied by the caller —
///    here: EOF on stdin, the most universal clean-exit stimulus);
/// 3. wait for the process to die and verify the final raw bytes RESTORE
///    every mode the app engaged (alt-screen leave, cursor show, mouse off,
///    paste off);
/// 4. relaunch and deliver SIGINT, then SIGTERM, recording whether each
///    teardown happened (a crash-path that leaves modes engaged is the
///    classic broken-terminal report).
///
/// Distinguishes the two outcomes the old audit conflated: "the APP
/// restored the terminal" (the final stream contains the resets) vs "the
/// PTY was destroyed so the HOST is fine but the app never cleaned up"
/// (no resets in the stream — the app failed its teardown duty even though
/// the user sees no damage).
pub fn lifecycle_exit_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    // ── Phase 1: the running app's engaged modes ──────────────────────────
    let Some((baseline_trace, _n, _d)) = decode_raw(session) else {
        findings.push(Finding {
            id: "LCX-NOSRC".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "engine retains no raw output; exit teardown evidence unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "the exit test needs the portable-pty engine" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    };
    let engaged: Vec<&'static str> = baseline_trace
        .modes
        .iter()
        // cursor_visible ENGAGED means the app HID the cursor (set=false);
        // its healthy teardown is a show. Every other engaged mode is a
        // set=true and its teardown is a reset. Pairing them here keeps the
        // cursor case in the verification set instead of silently dropping
        // it (a hidden-never-shown cursor is the classic invisible-cursor
        // bug after exit).
        .filter(|m| m.set || m.mode == "cursor_visible")
        .map(|m| m.mode)
        .collect();

    // ── Phase 2/3: normal exit + teardown verification ────────────────────
    // `quit\n` exits the cooperative fixture; any app whose exit stimulus is
    // typing works the same. The verification is on the FINAL byte range.
    let _ = session.send(crate::backend::Input::Text("quit\n".into()));
    let exit_wait = session.wait(crate::backend::WaitCond::ProcessExit, 5000);
    let exited = exit_wait.map(|o| o.met).unwrap_or(false);
    if !exited {
        findings.push(Finding {
            id: "LCX-NOEXIT".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "the app did not exit on the probe stimulus (quit + EOF); exit teardown untested — the standing dangling-mode audit still applies".into(),
            evidence: vec![ev_other(
                "exit_timeout",
                "process still running after the exit stimulus",
                json!({ "stimulus": "text quit + newline, 5s budget" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
        });
        return findings;
    }

    let (final_bytes, _cap, dropped) = {
        // The reader thread parked the exit bytes in pending ingest; wait()
        // does not drain it.
        session.absorb_ingest_now();
        // Audit P1-44: a failed teardown-window read means the teardown
        // analysis has no bytes to judge — report unrestored as UNKNOWN
        // (empty) rather than decoding an empty trace into "restored
        // nothing".
        match session.raw_output_window() {
            Ok(w) => w,
            Err(e) => {
                findings.push(Finding {
                    id: "TEARDOWN-WINDOW-UNREADABLE".into(),
                    rule_id: None,
                    severity: "warn".into(),
                    category: "lifecycle".into(),
                    summary: format!(
                        "raw output window read failed ({e}) — teardown mode restoration UNKNOWN, not verified"
                    ),
                    evidence: vec![ev_other(
                        "raw_ring_read_failed",
                        "the final-stream read errored",
                        json!({ "error": e.to_string() }),
                    )],
                    confidence: 1.0,
                    reproduction: None,
                    source_refs: Vec::new(),
                });
                (Vec::new(), 0, 0)
            }
        }
    };
    let final_trace = crate::protocol::ProtocolTrace::decode(&final_bytes);
    // Teardown map: for each engaged mode, did the final stream contain the
    // reset AFTER the app engaged it?
    let window_complete = dropped == 0;
    let mut restored: Vec<&str> = Vec::new();
    let mut unrestored: Vec<&str> = Vec::new();
    for mode in &engaged {
        if *mode == "cursor_visible" {
            // engaged hidden (set=false recorded as engaged-off); the
            // healthy teardown is a SHOW (set=true) after the hide.
            let showed = final_trace
                .modes
                .iter()
                .any(|m| m.mode == "cursor_visible" && m.set);
            if showed {
                restored.push(mode);
            } else {
                unrestored.push(mode);
            }
            continue;
        }
        let reset = final_trace.modes.iter().any(|m| m.mode == *mode && !m.set);
        if reset {
            restored.push(mode);
        } else {
            unrestored.push(mode);
        }
    }

    if !unrestored.is_empty() {
        findings.push(Finding {
            id: "LCX-TEARDOWN-MISSING".into(),
            rule_id: None,
            severity: "error".into(),
            category: "lifecycle".into(),
            summary: format!(
                "on clean exit the app did NOT restore {} engaged mode(s): {} — the app failed its teardown duty. (The host may still look fine because the PTY was destroyed; that is the host's mercy, not the app's correctness.)",
                unrestored.len(),
                unrestored.join(", ")
            ),
            evidence: vec![ev_other(
                "exit_teardown_missing",
                "engaged modes without a reset in the final stream",
                json!({
                    "engaged": engaged,
                    "restored": restored,
                    "unrestored": unrestored,
                    "final_stream_bytes": final_bytes.len(),
                    "window_complete": window_complete,
                }),
            )],
            confidence: if window_complete { 0.95 } else { 0.6 },
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else if !engaged.is_empty() {
        findings.push(Finding {
            id: "LCX-TEARDOWN-OK".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: format!(
                "clean exit restored every engaged mode: {} — the app performed its own terminal restoration.",
                engaged.join(", ")
            ),
            evidence: vec![ev_other(
                "exit_teardown_complete",
                "every engaged mode has a reset in the final stream",
                json!({
                    "engaged": engaged,
                    "restored": restored,
                    "app_restored_terminal": true,
                    "window_complete": window_complete,
                }),
            )],
            confidence: if window_complete { 0.95 } else { 0.6 },
            reproduction: None,
            source_refs: Vec::new(),
        });
    } else {
        findings.push(Finding {
            id: "LCX-NOTHING-ENGAGED".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: "the app engaged no restorable terminal modes during its run; exit teardown is trivially satisfied.".into(),
            evidence: vec![ev_other(
                "no_modes_engaged",
                "no DECSET-mode was engaged in the observed window",
                json!({ "window_complete": window_complete }),
            )],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    // ── Phase 4: signals, on the relaunched app ───────────────────────────
    // SIGINT/SIGTERM behavior is a separate lifecycle: a cooperative app
    // traps them and restores; a naive one dies leaving modes engaged. We
    // can only report what happened — a signal kill with no resets in the
    // stream is reported as an app-side observation, severity info when the
    // app never opted to handle signals.
    let eng_by_signal: Vec<serde_json::Value> = Vec::new();
    for sig in [2, 15] {
        if session.restart().is_err() {
            findings.push(Finding {
                id: "LCX-NORESTART".into(),
                rule_id: None,
                severity: "info".into(),
                category: "lifecycle".into(),
                summary: format!(
                    "session is not restartable; signal {sig} teardown untested (relaunch failed)"
                ),
                evidence: vec![ev_other(
                    "restart_failed",
                    "the session could not relaunch the target",
                    json!({ "signal": sig }),
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
            });
            break;
        }
        let _ = session.observe(200);
        let _ = session.send(crate::backend::Input::Signal(sig));
        let died = session
            .wait(crate::backend::WaitCond::ProcessExit, 3000)
            .map(|o| o.met)
            .unwrap_or(false);
        if !died {
            findings.push(Finding {
                id: "LCX-SIGNAL-IGNORED".into(),
                rule_id: None,
                severity: "info".into(),
                category: "lifecycle".into(),
                summary: format!(
                    "the app survived SIG{sig} — it traps the signal (a full-screen TUI commonly ignores or handles it). Not a defect; recorded as observed behavior."
                ),
                evidence: vec![ev_other(
                    "signal_survived",
                    "process alive 3s after signal delivery",
                    json!({ "signal": sig }),
                )],
                confidence: 0.9,
                reproduction: None,
                source_refs: Vec::new(),
            });
            // Stop the relaunched app so the session is not left running.
            let _ = session.stop();
            continue;
        }
        let (bytes_s, _c, dropped_s) = {
            session.absorb_ingest_now();
            // Audit P1-44: an unreadable window is an honest empty window
            // (0 resets claimed) only with the failure said out loud.
            match session.raw_output_window() {
                Ok(w) => w,
                Err(e) => {
                    findings.push(Finding {
                        id: "LCX-SIGNAL-WINDOW-UNREADABLE".into(),
                        rule_id: None,
                        severity: "warn".into(),
                        category: "lifecycle".into(),
                        summary: format!(
                            "raw output window read failed after SIG{sig} ({e}) — final-stream mode resets UNKNOWN"
                        ),
                        evidence: vec![ev_other(
                            "raw_ring_read_failed",
                            "post-signal stream read errored",
                            json!({ "signal": sig, "error": e.to_string() }),
                        )],
                        confidence: 1.0,
                        reproduction: None,
                        source_refs: Vec::new(),
                    });
                    (Vec::new(), 0, 0)
                }
            }
        };
        let trace_s = crate::protocol::ProtocolTrace::decode(&bytes_s);
        let resets = trace_s.modes.iter().filter(|m| !m.set).count();
        findings.push(Finding {
            id: "LCX-SIGNAL-EXIT".into(),
            rule_id: None,
            severity: "info".into(),
            category: "lifecycle".into(),
            summary: format!(
                "SIG{sig} terminated the app; the final stream contained {resets} mode reset(s). No resets means the signal path skipped terminal restoration (common for untrapped default handlers)."
            ),
            evidence: vec![ev_other(
                "signal_exit",
                "signal-driven exit teardown evidence",
                json!({
                    "signal": sig,
                    "mode_resets_in_final_stream": resets,
                    "window_complete": dropped_s == 0,
                    "eng_by_signal": eng_by_signal,
                }),
            )],
            confidence: 0.8,
            reproduction: None,
            source_refs: Vec::new(),
        });
    }

    findings
}
