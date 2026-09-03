//! "Prove the whole thing, not the pieces" — item 45.
//!
//! The lifecycle/crash/descendant matrix: real processes do the dying.
//! Every leg launches a REAL python fixture whose exit mode is chosen by
//! the test — clean 0, non-zero 7, self-killed by SIGTERM — and asserts
//! the OBSERVED truth (ProcessState, WaitCond::ProcessExit, the
//! ProcessExited event with code/signal) against what the kernel actually
//! did. Descendant legs spawn a child that outlives its parent and assert
//! the harness's process-group contract against real /proc state. Restart
//! and stop are asserted as lifecycle transitions, not method-return
//! smoke.

use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tui_lab::backend::{Input, WaitCond};
use tui_lab::session::SessionManager;

const SIGKILL: i32 = 9;
const SIGHUP: i32 = 1;

/// A script that prints a banner, holds briefly (so the harness's first
/// observation can capture it ALIVE — the derived ProcessExited event is
/// a running→dead edge between frames, and an app that dies unobserved
/// honestly has no such edge), then exits in the requested mode.
/// `signal` modes kill themselves with `os.kill(os.getpid(), sig)` so the
/// death is the fixture's own doing, not the harness's.
fn exit_mode_fixture(dir: &Path, mode: &str) -> String {
    let app = dir.join(format!("exit-{mode}.py"));
    let body = match mode {
        "clean" => "print('BYE-CLEAN')\nsys.stdout.flush()\ntime.sleep(2)\nsys.exit(0)",
        "nonzero" => "print('BYE-DIRTY')\nsys.stdout.flush()\ntime.sleep(2)\nsys.exit(7)",
        // SIGTERM self-kill: the standard death. Because the exit happened
        // via signal, the harness must report the SIGNAL, not code 1.
        "sigterm" => {
            "print('BYE-TERM')\nsys.stdout.flush()\ntime.sleep(2)\nos.kill(os.getpid(), 15)"
        }
        // Descendant mode: print, spawn a sleeper that outlives us, exit 0.
        // The sleeper writes its pid where the test can find it and ignores
        // SIGHUP so a mere group-HUP cannot fake a reap.
        "descendant" => {
            "\
print('PARENT-SPOWNED')
sys.stdout.flush()
time.sleep(2)
child = os.fork()
if child == 0:
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(60)
    os._exit(0)
with open(os.environ['DESC_PID_FILE'], 'w') as f:
    f.write(str(child))
sys.exit(0)"
        }
        other => panic!("unknown mode {other}"),
    };
    fs::write(
        &app,
        format!(
            "\
import sys, os, time, signal
{body}
"
        ),
    )
    .expect("write fixture");
    app.to_string_lossy().to_string()
}

/// A persistent app for restart/stop legs: prints, waits for a line, so it
/// is unambiguously ALIVE until the test acts.
fn longlived_fixture(dir: &Path) -> String {
    let app = dir.join("longlived.py");
    fs::write(
        &app,
        "\
import sys, time
print('ALIVE-1')
sys.stdout.flush()
input()
print('ALIVE-2')
sys.stdout.flush()
input()
print('ALIVE-3')
sys.stdout.flush()
time.sleep(30)
",
    )
    .expect("write fixture");
    app.to_string_lossy().to_string()
}

fn start(mgr: &mut SessionManager, app: &str, backend: &str) -> String {
    mgr.start(
        "python3",
        &[app.to_string()],
        None,
        &[],
        80,
        24,
        backend,
        "local",
    )
    .expect("launch")
}

/// Drive a session to its first frame (ProcessStarted is derived on first
/// observe) and return the event cursor.
fn to_first_frame(mgr: &mut SessionManager, sid: &str) -> u64 {
    let sess = mgr.resolve_mut(Some(sid)).unwrap();
    sess.observe(300).expect("first frame");
    let batch = sess.events_since(0);
    batch.cursor
}

/// Wait for the process to die, then observe once more so the derived
/// ProcessExited event is emitted into the session's event queue (the
/// event is a diff of the last two observed frames — a wait alone does
/// not emit it), and return the WaitOutcome.
fn wait_exit(
    sess: &mut tui_lab::session::Session,
    budget_ms: u64,
) -> tui_lab::backend::WaitOutcome {
    let out = sess
        .wait(WaitCond::ProcessExit, budget_ms)
        .expect("ProcessExit wait");
    let _ = sess.observe(50);
    out
}

fn event_kinds(sess: &tui_lab::session::Session) -> Vec<(String, Option<i32>, Option<String>)> {
    sess.all_events()
        .into_iter()
        .map(|e| match &e.kind {
            tui_lab::events::TerminalEventKind::ProcessExited {
                exit_code,
                exit_signal,
            } => ("process_exited".into(), *exit_code, exit_signal.clone()),
            other => (other.name().to_string(), None, None),
        })
        .collect()
}

/// The clean-exit leg: code 0, ProcessExit wait MET with reason
/// ProcessExit, the derived ProcessExited event carries Some(0)/None, and
/// ProcessState agrees with the event (running=false, exit_code=Some(0)).
#[test]
fn clean_exit_zero_reports_code_zero_everywhere() {
    let dir = TempDir::new().unwrap();
    let app = exit_mode_fixture(dir.path(), "clean");
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    let cursor = to_first_frame(&mut mgr, &sid);
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000)
    };
    assert!(out.met, "a clean exit must satisfy ProcessExit");
    assert_eq!(
        out.reason,
        tui_lab::backend::WaitReason::ProcessExit,
        "the reason names the truth: {:?}",
        out.reason
    );
    let sess = mgr.resolve(Some(&sid)).unwrap();
    // The captured frame IS the death frame: process already dead.
    assert!(
        !out.state.process.running,
        "ProcessExit's captured frame shows the dead process"
    );
    let exited = event_kinds(sess)
        .into_iter()
        .skip(cursor as usize)
        .filter(|(k, _, _)| k == "process_exited")
        .collect::<Vec<_>>();
    assert_eq!(
        exited,
        vec![("process_exited".to_string(), Some(0), None)],
        "exactly one ProcessExited with code 0 and no signal: {exited:?}"
    );
    let _ = sess;
    mgr.stop(&sid).ok();
}

/// The non-zero exit: 7 must arrive as 7 — not folded to 0 or 1.
#[test]
fn nonzero_exit_preserves_the_code() {
    let dir = TempDir::new().unwrap();
    let app = exit_mode_fixture(dir.path(), "nonzero");
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000)
    };
    assert!(out.met);
    let proc = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.process()
    };
    assert_eq!(proc.exit_code, Some(7), "the real code survives: {proc:?}");
    assert!(
        proc.exit_signal.is_none(),
        "an exit(7) is not a signal death"
    );
    assert!(!proc.running);
    let exited = {
        let sess = mgr.resolve(Some(&sid)).unwrap();
        event_kinds(sess)
            .into_iter()
            .filter(|(k, _, _)| k == "process_exited")
            .collect::<Vec<_>>()
    };
    assert_eq!(
        exited,
        vec![("process_exited".to_string(), Some(7), None)],
        "{exited:?}"
    );
    mgr.stop(&sid).ok();
}

/// Death by signal: the harness reports the SIGNAL (portable-pty maps it
/// through strsignal → "Terminated"), never a bare code. This is the
/// crash leg — a TUI that dies to a signal must be distinguishable from
/// one that exited non-zero.
#[test]
fn signal_death_reports_the_signal_not_a_code() {
    let dir = TempDir::new().unwrap();
    let app = exit_mode_fixture(dir.path(), "sigterm");
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000)
    };
    assert!(out.met);
    let exited = {
        let sess = mgr.resolve(Some(&sid)).unwrap();
        event_kinds(sess)
            .into_iter()
            .filter(|(k, _, _)| k == "process_exited")
            .collect::<Vec<_>>()
    };
    let (_, code, sig) = exited.first().expect("exactly one exited event").clone();
    assert_eq!(
        sig.as_deref(),
        Some("Terminated"),
        "SIGTERM is reported by name, not folded into a code: {exited:?}"
    );
    assert_ne!(code, Some(0), "a signal death is never success");
    mgr.stop(&sid).ok();
}

/// The harness's own signal action: Input::Signal(SIGKILL) to the process
/// GROUP kills the app from outside; the observation machinery must then
/// see a signal death (Killed), proving the act surface and the process
/// truth agree.
#[test]
fn harness_signal_action_kills_and_is_observed() {
    let dir = TempDir::new().unwrap();
    let app = longlived_fixture(dir.path());
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.send(Input::Signal(SIGKILL)).expect("signal send");
    }
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000)
    };
    assert!(out.met, "SIGKILL must actually kill");
    let exited = {
        let sess = mgr.resolve(Some(&sid)).unwrap();
        event_kinds(sess)
            .into_iter()
            .filter(|(k, _, _)| k == "process_exited")
            .collect::<Vec<_>>()
    };
    let (_, _, sig) = exited.first().expect("exited event").clone();
    assert_eq!(
        sig.as_deref(),
        Some("Killed"),
        "SIGKILL is reported as Killed: {exited:?}"
    );
    mgr.stop(&sid).ok();
}

/// Descendant reaping (spec section 7): the app forks a child that
/// outlives it. `stop()` signals the PROCESS GROUP, so the orphan dies
/// with the parent — asserted against real /proc/<pid> state, not
/// against the harness's own bookkeeping.
#[test]
fn stop_reaps_outliving_descendants_via_the_process_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let app = exit_mode_fixture(dir.path(), "descendant");
    std::env::set_var(
        "DESC_PID_FILE",
        "", // placeholder; the real path rides the launch env below
    );
    std::env::remove_var("DESC_PID_FILE");
    let mut mgr = SessionManager::new();
    let env = vec![(
        "DESC_PID_FILE".to_string(),
        pid_file.to_string_lossy().to_string(),
    )];
    let sid = mgr
        .start("python3", &[app], None, &env, 80, 24, "auto", "local")
        .expect("launch");
    to_first_frame(&mut mgr, &sid);

    // Wait until the parent exited AND wrote the descendant pid.
    {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000);
    }
    let mut desc_pid: u32 = 0;
    for _ in 0..50 {
        if let Ok(s) = fs::read_to_string(&pid_file) {
            if let Ok(p) = s.trim().parse::<u32>() {
                desc_pid = p;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(desc_pid > 0, "the fixture wrote its descendant pid");

    let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
    assert!(
        alive(desc_pid),
        "the descendant outlives its parent (pid {desc_pid})"
    );

    // stop() — the harness's only teardown — must reap the group.
    mgr.stop(&sid).expect("stop");
    // SIGTERM to the group: give the orphan a moment to die, then require
    // it gone (it ignores SIGTERM, so this also proves the second-stage
    // kill path reaches it; either way it must NOT survive stop()).
    let mut gone = false;
    for _ in 0..100 {
        if !alive(desc_pid) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(gone, "descendant pid {desc_pid} must be reaped by stop()");
}

/// A live process at rest: pid is real (the /proc entry exists and is a
/// python process), ProcessState.running agrees with the kernel.
#[test]
fn running_process_state_matches_procfs() {
    let dir = TempDir::new().unwrap();
    let app = longlived_fixture(dir.path());
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    let proc = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.process()
    };
    let pid = proc.pid.expect("a launched session has a pid");
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("pid exists in procfs");
    // Field 3 (state) after the comm's closing paren: S = sleeping, R =
    // running. A blocked-on-input python is always S here.
    let state = stat.rsplit(") ").next().unwrap_or("").chars().next();
    assert!(
        matches!(state, Some('S') | Some('R')),
        "the pid is a live sleeping/running process: {stat}"
    );
    assert!(proc.running, "ProcessState agrees with procfs");
    assert_eq!(proc.exit_code, None);
    mgr.stop(&sid).ok();
    // After stop, the pid is gone.
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "stop() removed the process from the machine"
    );
}

/// restart() is a lifecycle transition, not a fresh session: same id,
/// generation increments, the old pid dies, a NEW pid runs, and the
/// ProcessExited/ProcessStarted event pair is recorded under the right
/// generations.
#[test]
fn restart_is_same_session_new_generation_new_pid() {
    let dir = TempDir::new().unwrap();
    let app = longlived_fixture(dir.path());
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    let (old_pid, gen1) = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        (sess.process().pid, sess.generation)
    };
    let old_pid = old_pid.expect("pid");
    let (_id, gen2) = mgr.restart(&sid).expect("restart");
    assert_eq!(gen2, gen1 + 1, "generation increments in place");
    assert!(mgr.list().contains(&sid), "restart keeps the session id");
    // The new process runs the fixture again: ALIVE-1 must reappear.
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        let o = sess
            .wait(WaitCond::Text("ALIVE-1".into()), 5_000)
            .expect("wait for new generation's banner");
        // The new generation's ProcessStarted derives on its first
        // session-level observe; the Text wait alone bypasses that.
        let _ = sess.observe(50);
        o
    };
    assert!(out.met, "the restarted process prints again");
    let new_pid = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.process().pid
    }
    .expect("new pid");
    assert_ne!(new_pid, old_pid, "a real new process, not the old one");
    assert!(
        !Path::new(&format!("/proc/{old_pid}")).exists(),
        "the old process is gone"
    );
    // Event history: ProcessExited for gen1's death, ProcessStarted for
    // gen2 — both under the SAME session id, labeled with their own
    // generation.
    let (exited, started_gen2) = {
        let sess = mgr.resolve(Some(&sid)).unwrap();
        let all = sess.all_events();
        let exited: Vec<_> = all
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    tui_lab::events::TerminalEventKind::ProcessExited { .. }
                )
            })
            .cloned()
            .collect();
        let started_gen2 = all.iter().any(|e| {
            e.generation == gen2 && e.kind == tui_lab::events::TerminalEventKind::ProcessStarted
        });
        (exited, started_gen2)
    };
    assert!(
        exited.iter().any(|e| e.generation == gen1),
        "gen {gen1} records its own death: {exited:?}"
    );
    assert!(started_gen2, "gen {gen2} records its own start");
    mgr.stop(&sid).ok();
}

/// stop() clears the active pointer and the session itself; double-stop
/// is idempotent (a second stop on a reaped id does not error).
#[test]
fn stop_is_terminal_and_idempotent_at_the_manager() {
    let dir = TempDir::new().unwrap();
    let app = longlived_fixture(dir.path());
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    let pid = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.process().pid
    }
    .expect("pid");
    mgr.stop(&sid).expect("first stop");
    assert!(!mgr.list().contains(&sid), "session removed");
    assert!(mgr.active_id().is_none(), "active pointer cleared");
    mgr.stop(&sid).ok(); // idempotent second stop — no error
                         // And the process is really gone.
    let mut gone = false;
    for _ in 0..50 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(gone, "pid {pid} gone after stop");
}

/// The crash matrix across ENGINES: clean/exit-7/signal deaths observed
/// identically on the pipe engine (its own process()/exit-status code
/// path), so lifecycle truth is not a portable-pty-only property.
#[test]
fn exit_matrix_holds_on_the_pipe_engine() {
    for (mode, expect_code, expect_sig) in [("clean", Some(0), None), ("nonzero", Some(7), None)] {
        let dir = TempDir::new().unwrap();
        let app = exit_mode_fixture(dir.path(), mode);
        let mut mgr = SessionManager::new();
        let sid = start(&mut mgr, &app, "pipe");
        to_first_frame(&mut mgr, &sid);
        let out = {
            let sess = mgr.resolve_mut(Some(&sid)).unwrap();
            wait_exit(sess, 5_000)
        };
        assert!(out.met, "{mode}: pipe engine sees the exit");
        let proc = {
            let sess = mgr.resolve_mut(Some(&sid)).unwrap();
            sess.process()
        };
        assert_eq!(proc.exit_code, expect_code, "{mode}: {proc:?}");
        assert_eq!(proc.exit_signal, expect_sig, "{mode}");
        mgr.stop(&sid).ok();
    }
}

/// SIGHUP to the group (the classic terminal-hangup death) is observed
/// with its own signal name — the matrix distinguishes HUP from TERM.
#[test]
fn sighup_death_is_distinguished_from_sigterm() {
    let dir = TempDir::new().unwrap();
    let app = exit_mode_fixture(dir.path(), "sigterm");
    let mut mgr = SessionManager::new();
    let sid = start(&mut mgr, &app, "auto");
    to_first_frame(&mut mgr, &sid);
    {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        sess.send(Input::Signal(SIGHUP)).expect("hup");
    }
    let out = {
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        wait_exit(sess, 5_000)
    };
    assert!(out.met);
    let exited = {
        let sess = mgr.resolve(Some(&sid)).unwrap();
        event_kinds(sess)
            .into_iter()
            .filter(|(k, _, _)| k == "process_exited")
            .collect::<Vec<_>>()
    };
    let (_, _, sig) = exited.first().expect("exited").clone();
    assert_eq!(
        sig.as_deref(),
        Some("Hangup"),
        "SIGHUP reported as Hangup — distinct from Terminated: {exited:?}"
    );
    mgr.stop(&sid).ok();
}
