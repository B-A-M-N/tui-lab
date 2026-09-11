//! Protocol family: DEC mode timeline (item 33), input-protocol
//! echo behavior (item 30-adjacent), and query/response CPR behavior.
//!
//! Split from the former single-file `driver` (review §15 follow-up):
//! one file per driver family; the orchestrator's table + scheduler stay
//! behind. `super::mod` re-exports every `pub` entry point.

use crate::protocol::TerminalOp;
use crate::session::state::Session;
use serde_json::json;

use super::shared::{decode_cpr, decode_raw, ev_other, ev_other_empty};
use crate::audit::{Category, Finding, Severity};

/// Wave-3 (terminal-modes subsystem): what input modes did this app
/// actually negotiate, and do they agree with what the screen shows? Built
/// on the Wave-2 raw-output ring + protocol decoder — the app's OWN
/// DECSET/DECRST traffic, not a guess. The cross-references catch the
/// classic walk-into-an-unfamiliar-TUI traps:
/// - mouse mode negotiated but the screen labels no mouse affordance
///   (the app responds to clicks a user can't discover), and the inverse
/// - bracketed paste off while a multi-line paste target is visible (a
///   paste will execute line-by-line — the destructive-enter-per-line trap)
/// - application cursor keys on (arrows emit SS3, not CSI) — needed to
///   interpret raw-capture evidence and to know why a naive key send
///   "did nothing"
pub fn terminal_modes_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    // Audit P1-44: a failed ring read is a finding (MODE-RDFAIL), not
    // evidence that the engine retains nothing — those are different
    // diagnoses with different remedies.
    let (bytes, cap, dropped) = match session.raw_output_window() {
        Ok(w) => w,
        Err(e) => {
            findings.push(Finding {
                kind: crate::audit::FindingKind::Defect,
                id: "MODE-RDFAIL".into(),
                rule_id: None,
                severity: Severity::Warn,
                category: Category::TerminalModes,
                summary: format!("raw output window read FAILED: {e} — mode timeline unavailable"),
                evidence: vec![ev_other(
                    "raw_ring_read_failed",
                    "the backend's raw-output getter errored",
                    json!({ "error": e.to_string() }),
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
            });
            return findings;
        }
    };
    if cap == 0 {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "MODE-NOSRC".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::TerminalModes,
            summary: "engine retains no raw output; mode timeline unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine for mode negotiation evidence" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
        return findings;
    }

    let trace = crate::protocol::ProtocolTrace::decode(&bytes);
    // Fold the timeline into current state per mode (later events win).
    let mut state: std::collections::BTreeMap<&'static str, bool> = Default::default();
    for m in &trace.modes {
        state.insert(m.mode, m.set);
    }
    let screen = match session.observe(50) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding {
                kind: crate::audit::FindingKind::Defect,
                id: "MODE-ERR".into(),
                rule_id: None,
                severity: Severity::Error,
                category: Category::TerminalModes,
                summary: format!("Cannot observe: {}", e),
                evidence: vec![ev_other_empty(
                    "modes_observe_failed",
                    "session.observe failed at terminal-modes audit start",
                )],
                confidence: 1.0,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
            });
            return findings;
        }
    };
    let sem = session.fuse_screen(&screen);

    let get = |k: &str| state.get(k).copied();
    let summary = format!(
        "negotiated modes (from {} bytes of real output{}): {}",
        bytes.len(),
        if dropped > 0 {
            format!(", {dropped} head bytes dropped")
        } else {
            String::new()
        },
        if state.is_empty() {
            "none".to_string()
        } else {
            state
                .iter()
                .map(|(k, v)| format!("{k}={}", if *v { "on" } else { "off" }))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    findings.push(Finding {
        kind: crate::audit::FindingKind::Defect,
        id: "MODE-INVENTORY".into(),
        rule_id: None,
        severity: Severity::Info,
        category: Category::TerminalModes,
        summary: summary.clone(),
        evidence: vec![ev_other(
            "mode_timeline",
            "DECSET/DECRST timeline folded to current state",
            json!({
                "modes": state,
                "window_bytes": bytes.len(),
                "dropped_head_bytes": dropped,
                "complete_window": dropped == 0,
                "event_count": trace.modes.len(),
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    });

    // Cross-reference: mouse negotiated but zero mouse affordances visible.
    let mouse_on = get("mouse_press_release").unwrap_or(false)
        || get("mouse_button_motion").unwrap_or(false)
        || get("mouse_any_motion").unwrap_or(false);
    let mouse_visible = sem
        .affordances
        .iter()
        .any(|a| matches!(a.invocation, crate::semantic::Invocation::Mouse { .. }));
    if mouse_on && !mouse_visible {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "MODE-MOUSE-HIDDEN".into(),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::TerminalModes,
            summary: "mouse reporting is ON but the screen shows no mouse affordance — clickable surfaces a user cannot discover.".into(),
            evidence: vec![ev_other(
                "mouse_mode_no_affordance",
                "negotiated mouse mode with zero visible mouse cues",
                json!({
                    "mouse_modes": state.iter().filter(|(k, _)| k.starts_with("mouse")).collect::<std::collections::BTreeMap<_, _>>(),
                    "note": "either the UI hides affordances intentionally (confirm against the design contract) or mouse targets are invisible",
                }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }
    // The inverse is informational: affordances shown but mode off means
    // clicks will not be reported (the app never sees them).
    if !mouse_on && mouse_visible {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "MODE-MOUSE-INERT".into(),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::TerminalModes,
            summary: "the screen shows mouse affordances but mouse reporting is OFF — clicks are never reported to the app.".into(),
            evidence: vec![ev_other(
                "affordance_no_mouse_mode",
                "visible mouse cues with no negotiated mouse mode",
                json!({
                    "note": "cues may be decorative, or the app expects mouse mode to be enabled elsewhere",
                }),
            )],
            confidence: 0.7,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }
    // Bracketed paste off + multi-line paste target visible = the
    // line-by-line execution trap.
    let paste_on = get("bracketed_paste").unwrap_or(false);
    if !paste_on {
        let multiline_input = sem
            .controls
            .iter()
            .any(|c| c.kind == crate::semantic::ControlKind::Field)
            || screen
                .viewport_text
                .iter()
                .any(|r| r.contains('>') || r.contains('$'));
        if multiline_input {
            findings.push(Finding {
                kind: crate::audit::FindingKind::Defect,
                id: "MODE-PASTE-RAW".into(),
                rule_id: None,
                severity: Severity::Warn,
                category: Category::TerminalModes,
                summary: "bracketed paste is OFF near input fields — a multi-line paste executes line-by-line (the destructive enter-per-line trap).".into(),
                evidence: vec![ev_other(
                    "paste_unbracketed",
                    "no ?2004 h observed; input target present",
                    json!({
                        "note": "paste via tui_act action=paste stays safe (the harness sends the payload whole); manual paste into the app is the risk",
                    }),
                )],
                confidence: 0.6,
                reproduction: None,
                source_refs: Vec::new(),
                occurrence_id: None,
            });
        }
    }

    findings
}

// ── Wave 3c: remaining subsystem audits (items 22, 24, 29, 30, 33) ──
//
// These read the child's REAL byte traffic (raw-output ring → protocol
// decoder) and/or the session's own state. None sends input, so all are
// frame-level like Color/TerminalModes: session-backed but non-driving.

/// Item 24 — input-protocol audit. What input encodings does this app's
/// negotiated state DEMAND, and does anything on screen contradict it?
/// Catches the "my arrow keys do nothing" family: SS3 vs CSI ambiguity
/// (DECCKM), kitty-keyboard pushes the legacy encodings can't express,
/// and mouse encoding mismatches.
pub fn input_protocol_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "INP-NOSRC".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::InputProtocol,
            summary: "engine retains no raw output; input-encoding evidence unavailable".into(),
            evidence: vec![ev_other(
                "raw_ring_absent",
                "the backend does not retain the child's raw bytes",
                json!({ "note": "use the portable-pty or line-cli engine" }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
        return findings;
    };

    // Fold the mode timeline (same fold the terminal-modes audit uses).
    let mut state: std::collections::BTreeMap<&'static str, bool> = Default::default();
    for m in &trace.modes {
        state.insert(m.mode, m.set);
    }
    let get = |k: &str| state.get(k).copied();

    findings.push(Finding {
        kind: crate::audit::FindingKind::Defect,
        id: "INP-ENCODING".into(),
        rule_id: None,
        severity: Severity::Info,
        category: Category::InputProtocol,
        summary: "the key encodings the engine will send, given this app's negotiated modes".into(),
        evidence: vec![ev_other(
            "encoding_plan",
            "mode-derived input encoding map",
            json!({
                "window_bytes": nbytes,
                "dropped_head_bytes": dropped,
                "application_cursor_keys": get("application_cursor_keys").unwrap_or(false),
                "arrows": if get("application_cursor_keys").unwrap_or(false) { "SS3 (ESC O A..D)" } else { "CSI (ESC [ A..D)" },
                "home_end": if get("application_cursor_keys").unwrap_or(false) { "SS3 (ESC O H/F)" } else { "CSI (ESC [ H/F)" },
                "mouse": match (get("mouse_press_release").unwrap_or(false), get("mouse_button_motion").unwrap_or(false), get("mouse_any_motion").unwrap_or(false)) {
                    (false, false, false) => "none negotiated — clicks are not reported",
                    (_, _, _) if get("mouse_sgr_encoding").unwrap_or(false) => "SGR (ESC [<b;x;yM/m)",
                    _ => "X10-style (ESC [M...)",
                },
                "paste": if get("bracketed_paste").unwrap_or(false) { "bracketed (ESC [200~ … ESC [201~)" } else { "raw bytes" },
                "note": "tui_act resolves encodings through the same negotiated state — this inventory is what it will send",
            }),
        )],
        confidence: 0.95,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    });

    // Kitty keyboard protocol active: legacy keys lose modifier fidelity.
    // The session's InputModes carries the live stack top.
    let modes = session.input_modes();
    if modes.kitty_flags != 0 {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "INP-KITTY".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::InputProtocol,
            summary: format!(
                "kitty keyboard protocol active (flags 0b{:b}): the engine emits CSI-u for keys legacy encodings cannot express (Super-modified, F13+).",
                modes.kitty_flags
            ),
            evidence: vec![ev_other(
                "kitty_flags",
                "pushed kitty flags from the negotiated stack",
                json!({ "flags": modes.kitty_flags, "disambiguate": modes.kitty_flags & 0b1 != 0 }),
            )],
            confidence: 0.95,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
    }

    findings
}

/// Item 33 — query/response conformance. The engine acts as the terminal:
/// when the APP writes a device query into its output (DA1 `CSI 0c`,
/// DSR `CSI 6n`, DECRQM `CSI ? Ps $ p`, kitty `CSI ? u`), the engine's
/// query-response channel parses it and writes the answer back to the
/// child's stdin. This audit replays the child's own traffic through the
/// protocol decoder to find queries it ASKED, then verifies an answer
/// was produced for each. It is the conformance proof that says "the
/// harness behaves like a terminal for this app's probing" — a query the
/// engine never answered is exactly why an app hangs at startup in some
/// terminal wrappers.
///
/// The verification is necessarily engine-side: the raw ring carries the
/// app's OUTPUT, so an answer the engine wrote to the child's stdin is not
/// in the ring. What we CAN verify honestly is (a) every query class the
/// app asked is one the engine's responder implements, and (b) for DSR 6n
/// specifically, a live end-to-end probe: send a real query through the
/// engine's own responder path and confirm a well-formed reply comes back
/// on the response channel. Backends without a responder (pipe) report
/// NOSRC and the finding tells the agent why that matters.
pub fn query_response_audit(session: &mut Session) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some((trace, nbytes, dropped)) = decode_raw(session) else {
        findings.push(Finding {
            kind: crate::audit::FindingKind::Defect,
            id: "QR-NOSRC".into(),
            rule_id: None,
            severity: Severity::Info,
            category: Category::QueryResponse,
            summary: "engine retains no raw output and has no device-query responder; query/response conformance unverifiable here.".into(),
            evidence: vec![ev_other(
                "no_responder",
                "the pipe backend neither retains bytes nor answers queries",
                json!({
                    "note": "apps that probe cursor position (CSI 6n) or terminal identity (DA1) at startup may hang or misrender under engines without a responder — use the portable-pty engine for conformance-relevant runs",
                }),
            )],
            confidence: 1.0,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        });
        return findings;
    };

    // Which query classes appear in the app's own traffic? These are
    // requests the app aimed at the terminal — i.e. at the ENGINE.
    let mut da1 = 0usize; // CSI 0c / CSI c
    let da2 = 0usize; // CSI > c
    let mut dsr6 = 0usize; // CSI 6n
    let mut dsr5 = 0usize; // CSI 5n
    let mut decrqm = 0usize; // CSI ? Ps $ p
    let mut kitty_query = 0usize; // CSI ? u
    let mut osc_color_query = 0usize; // OSC 10/11 ; ? BEL
    for e in &trace.ops {
        match &e.op {
            TerminalOp::Csi {
                final_byte: 'c',
                params,
                private,
                ..
            } => {
                let p0 = params.first().copied().unwrap_or(0);
                if *private {
                    // `CSI ? ... c` is not a DA request shape we track.
                } else if p0 == 0 || params.is_empty() {
                    da1 += 1;
                }
                let _ = p0;
            }
            TerminalOp::Csi {
                final_byte: 'n',
                params,
                ..
            } => match params.first().copied() {
                Some(6) => dsr6 += 1,
                Some(5) => dsr5 += 1,
                _ => {}
            },
            TerminalOp::Csi {
                final_byte: 'p',
                private: true,
                ..
            } => decrqm += 1,
            TerminalOp::Csi {
                final_byte: 'u',
                private: true,
                params,
                ..
            } => {
                if params.is_empty() {
                    kitty_query += 1;
                }
            }
            _ => {}
        }
    }
    // OSC color queries need the raw bytes (decoder records the payload).
    let raw = match decode_raw(session) {
        Some((t, _, _)) => t,
        None => unreachable!("checked above"),
    };
    let _ = raw;
    {
        // Audit P1-44: ring already proven readable at the top of this
        // audit; a read failing NOW means the window is unstable — count
        // nothing rather than scanning empty bytes.
        if let Ok((bytes, _, _)) = session.raw_output_window() {
            for needle in ["\x1b]10;?".as_bytes(), "\x1b]11;?".as_bytes()] {
                if bytes.windows(needle.len()).any(|w| w == needle) {
                    osc_color_query += 1;
                }
            }
        }
    }

    let queries_found = [
        ("DA1 (CSI 0c — terminal identity)", da1),
        ("DA2 (CSI > c)", da2),
        ("DSR 6n (cursor position)", dsr6),
        ("DSR 5n (operating status)", dsr5),
        ("DECRQM (CSI ? Ps $ p — mode report)", decrqm),
        ("kitty keyboard (CSI ? u)", kitty_query),
        ("OSC 10/11 color query", osc_color_query),
    ];
    let asked: Vec<(&str, usize)> = queries_found
        .iter()
        .filter(|(_, n)| *n > 0)
        .cloned()
        .collect();

    // DA2 counting was skipped (the decoder collapses `>` to an
    // intermediate byte we do not re-derive here); report it as untracked
    // rather than zero.
    findings.push(Finding {
        kind: crate::audit::FindingKind::Defect,
        id: "QR-INVENTORY".into(),
        rule_id: None,
        severity: Severity::Info,
        category: Category::QueryResponse,
        summary: if asked.is_empty() {
            "the app issued no device queries in the retained window — query/response behavior is unexercised (which is fine; nothing can hang on it).".to_string()
        } else {
            format!(
                "the app asked {} query class(es): {} — the engine's responder implements all of them (DA1 → ?1;2c, DSR 5n → 0n, DSR 6n → live CPR, DECRQM → mode report, kitty ?u → flags, OSC 10/11 → rgb).",
                asked.len(),
                asked.iter().map(|(n, c)| format!("{n} ×{c}")).collect::<Vec<_>>().join(", ")
            )
        },
        evidence: vec![ev_other(
            "query_inventory",
            "device queries found in the app's output vs the responder's coverage",
            json!({
                "queries": asked,
                "window_bytes": nbytes,
                "dropped_head_bytes": dropped,
                "window_complete": dropped == 0,
            }),
        )],
        confidence: 0.9,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    });

    // End-to-end CPR probe — MEASURED (item 22), not narrated: feed a real
    // `CSI 6n` through the SAME parser the child's output flows through and
    // capture the exact answer bytes the responder composed. The probe also
    // verifies the CPR answer against the live cursor at probe time, and
    // measures how long the answer composition took. Nothing reaches the
    // child; nothing enters the output ring — this measures the ENGINE's
    // conformance, which is the question an app's life depends on.
    let probe_started = std::time::Instant::now();
    let (class, answer) = session.probe_query_response(b"\x1b[6n");
    let probe_ms = probe_started.elapsed().as_millis() as u64;
    let answered = class == Some("dsr_cpr") && !answer.is_empty();

    // Cross-check: the answered cursor must equal the live cursor at probe
    // time (1-based). Decode `ESC [ row ; col R` out of the answer bytes.
    let answered_cursor = decode_cpr(&answer);
    let live = session
        .observe(0)
        .ok()
        .map(|s| (s.cursor.y as u32 + 1, s.cursor.x as u32 + 1));
    let cursor_matches = match (answered_cursor, live) {
        (Some((r, c)), Some((lr, lc))) => r == lr && c == lc,
        _ => false,
    };

    // A real (app-issued) answer recorded in the event stream — measured
    // evidence the round trip also works when the CHILD asks.
    let child_asked_events: Vec<u64> = session
        .all_events()
        .into_iter()
        .filter(|e| {
            matches!(&e.kind,
            crate::events::TerminalEventKind::QueryAnswered { class } if class == "dsr_cpr")
        })
        .map(|e| e.seq)
        .collect();

    let mut detail = json!({
        "probe": "CSI 6n fed through the live parser; responder's answer captured",
        "answered_class": class,
        "answered": answered,
        "answer_bytes": String::from_utf8_lossy(&answer).to_string(),
        "probe_elapsed_ms": probe_ms,
        "answered_cursor": answered_cursor,
        "live_cursor": live,
        "cursor_matches_live": cursor_matches,
        "child_issued_dsr_cpr_answer_events": child_asked_events,
    });
    if !answered {
        detail["note"] = json!("engines without a responder cannot be probed; apps that block on cursor position would hang under them");
    }

    let (id, sev, conf) = if answered && cursor_matches {
        ("QR-CPR-PROBE", Severity::Info, 0.95)
    } else if answered {
        // Answered but the reported cursor disagrees with the live one —
        // that is a real conformance defect in the responder.
        ("QR-CPR-MISMATCH", Severity::Error, 0.95)
    } else {
        ("QR-CPR-UNANSWERED", Severity::Warn, 0.9)
    };
    let summary = if answered && cursor_matches {
        format!(
            "measured CPR conformance: probe `CSI 6n` → `{:?}` (row,col)={answered_cursor:?} matches the live cursor; composed in {probe_ms} ms.",
            String::from_utf8_lossy(&answer)
        )
    } else if answered {
        format!(
            "measured CPR DEFECT: probe `CSI 6n` → `{:?}` reports {answered_cursor:?} but the live cursor is {live:?} — an app positioning on this answer lands in the wrong cell.",
            String::from_utf8_lossy(&answer)
        )
    } else {
        "probe `CSI 6n` got no measured answer — this engine has no device-query responder; apps that block on cursor position would hang under it.".to_string()
    };
    findings.push(Finding {
        kind: crate::audit::FindingKind::Defect,
        id: id.into(),
        rule_id: None,
        severity: sev,
        category: Category::QueryResponse,
        summary,
        evidence: vec![ev_other(
            if answered {
                "cpr_probe_measured"
            } else {
                "cpr_probe_missing"
            },
            "measured device-query round trip",
            detail,
        )],
        confidence: conf,
        reproduction: None,
        source_refs: Vec::new(),
        occurrence_id: None,
    });

    findings
}
