//! "Prove the whole thing, not the pieces" — item 44.
//!
//! The terminal-profile matrix: the same cooperative fixture driven at
//! multiple sizes (including the 20x8 floor where layouts die) and under
//! different TERM/env configurations, with observations AND audits
//! asserted at every point — not only at the golden 80x24.

use std::fs;
use tempfile::TempDir;
use tui_lab::session::SessionManager;

/// A dialog fixture with fixed-width content so every profile point asks
/// the same question of the screen (do the controls stay found, does the
/// focus stay provable, does the audit machinery hold). It REDRAWS on
/// every key it consumes: audit drivers type probe bytes into the app,
/// and a fixture that never clears would bury its own content under the
/// probe noise — the matrix asserts the machinery, not a deaf screen.
fn fixture_dir() -> (TempDir, String) {
    let dir = TempDir::new().expect("fixture dir");
    let app = dir.path().join("dialog.py");
    fs::write(
        &app,
        "\
import sys, time, os, select, signal, termios, tty

gen = 0

def draw():
    global gen
    gen += 1
    # Explicit CR before every LF: raw mode has no OPOST, and after a
    # width clip the cursor can sit at the last column — a bare LF would
    # then print the next line OFF-SCREEN.
    out = '\\x1b[H\\x1b[2J[ Save ]   [ Cancel ]\\r\\nq quit\\r\\ng%d' % gen
    sys.stdout.write(out)
    sys.stdout.flush()

dirty = True
def on_winch(signum, frame):
    global dirty
    dirty = True

signal.signal(signal.SIGWINCH, on_winch)

fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
try:
    tty.setraw(fd)
except Exception:
    pass
while True:
    if dirty:
        dirty = False
        draw()
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        # Drain everything pending, then redraw once — a burst of probe
        # keys collapses into one clean frame. os.read (one syscall,
        # returns what is available) — BufferedReader.read(n) would BLOCK
        # until exactly n bytes and freeze the fixture mid-burst.
        try:
            while select.select([fd], [], [], 0.05)[0]:
                if not os.read(fd, 4096):
                    break
        except Exception:
            pass
        dirty = True

termios.tcsetattr(fd, termios.TCSADRAIN, saved)
",
    )
    .expect("write fixture");
    let path = app.to_string_lossy().to_string();
    (dir, path)
}

/// The fixture stamps every redraw with a generation counter (`g<N>` on
/// its own row). Waiting for the counter to ADVANCE is the anchored way
/// to prove a frame is a POST-resize redraw rather than a stale clipped
/// image of the old geometry.
fn screen_generation(screen: &tui_lab::screen::ScreenState) -> Option<u64> {
    screen
        .viewport_text
        .iter()
        .filter_map(|l| l.trim().strip_prefix('g'))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
}

fn start_at(mgr: &mut SessionManager, app: &str, cols: u16, rows: u16, env: &[(String, String)]) -> String {
    mgr.start(
        "python3",
        &[app.to_string()],
        None,
        env,
        cols,
        rows,
        "auto",
        "local",
    )
    .expect("start fixture")
}

/// The per-point contract: the observed frame is a FRESH redraw of the
/// requested geometry (anchored via the fixture's generation counter, so
/// a stale pre-resize frame can never pass), the text is present, and
/// the static audits run with resolvable evidence.
fn assert_profile_holds(
    mgr: &mut SessionManager,
    sid: &str,
    cols: u16,
    rows: u16,
    min_gen: u64,
    label: &str,
) {
    // Anchor: poll until the fixture has redrawn at generation >= min_gen
    // AT the requested geometry — proves the frame postdates the profile
    // change rather than trusting observe to have raced a redraw.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let sess = mgr.resolve_mut(Some(sid)).unwrap();
    let screen = loop {
        let screen = sess.observe(300).expect("observe");
        if screen_generation(&screen).is_some_and(|g| g >= min_gen)
            && (screen.cols, screen.rows) == (cols, rows)
        {
            break screen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label}: fixture never produced generation >= {min_gen} at {cols}x{rows}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let joined = screen.viewport_text.join("\n");
    assert!(
        joined.contains("[ Save ]") && joined.contains("q quit"),
        "{label}: content must render at {cols}x{rows}: {joined:?}"
    );

    // Static audit machinery holds at every size.
    let report = tui_lab::audit::orchestrator::run_profile_checked(
        sess,
        "full",
        None,
        tui_lab::audit::orchestrator::SafetyPolicy::AllowMutation,
    )
    .unwrap_or_else(|e| panic!("{label}: full audit must run at {cols}x{rows}: {e}"));
    assert!(
        !report.findings.is_empty(),
        "{label}: an audit with findings machinery working yields findings"
    );
    for f in &report.findings {
        assert!(
            !f.evidence.is_empty(),
            "{label}: evidence contract holds at {cols}x{rows}"
        );
    }
    // The resize driver restores the requested geometry (item 65 residue
    // contract): after the full audit, the session is back at cols x rows.
    let after = sess.observe(60).expect("post-audit observe");
    assert_eq!(
        (after.cols, after.rows),
        (cols, rows),
        "{label}: resize matrix inside the audit must restore geometry"
    );
}

/// The size matrix: floor, classic, wide. Same fixture, same assertions.
#[test]
fn size_matrix_floor_classic_and_wide() {
    let (_dir, app) = fixture_dir();
    let mut mgr = SessionManager::new();
    for (cols, rows, label) in [
        (20u16, 8u16, "floor 20x8"),
        (80, 24, "classic 80x24"),
        (120, 40, "wide 120x40"),
    ] {
        let sid = start_at(&mut mgr, &app, cols, rows, &[]);
        assert_profile_holds(&mut mgr, &sid, cols, rows, 1, label);
        mgr.stop(&sid).ok();
    }
}

/// The env matrix: TERM variations and a scrubbed environment. The
/// observations and audit machinery must hold under each; the frame's
/// process stays alive (a TERM the app cannot use must not silently kill
/// the session).
#[test]
fn env_matrix_term_variants_and_scrubbed_env() {
    let (_dir, app) = fixture_dir();
    let mut mgr = SessionManager::new();
    type EnvCase = (String, &'static str, Vec<(String, String)>);
    let envs: Vec<EnvCase> = vec![
        ("dumb".into(), "TERM=dumb", vec![("TERM".into(), "dumb".into())]),
        (
            "xterm-256color".into(),
            "TERM=xterm-256color",
            vec![("TERM".into(), "xterm-256color".into())],
        ),
        (
            "vt100".into(),
            "TERM=vt100",
            vec![("TERM".into(), "vt100".into())],
        ),
    ];
    for (term, label, env) in envs {
        let sid = start_at(&mut mgr, &app, 80, 24, &env);
        assert_profile_holds(&mut mgr, &sid, 80, 24, 1, label);
        // The child survived its TERM.
        let sess = mgr.resolve(Some(&sid)).unwrap();
        assert!(
            sess.adapter_status().adapter_available,
            "{label}: session stays instrumented under {term}"
        );
        mgr.stop(&sid).ok();
    }

    // Scrubbed environment (clean isolation): fresh scratch HOME/TMPDIR,
    // no inherited config — which scrubs PATH too, so the interpreter must
    // be named by absolute path (that is the harness's own contract: clean
    // isolation = no environment trust). The fixture needs nothing from the
    // world; the harness must hold under it.
    let python3 = std::env::var("PYTHON3").unwrap_or_else(|_| "python3".into());
    let python3: String = {
        // Resolve once via the absolute path of the working interpreter.
        let out = std::process::Command::new("which")
            .arg(&python3)
            .output()
            .expect("which python3");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert!(
        !python3.is_empty() && python3.starts_with('/'),
        "python3 must resolve to an absolute path for clean isolation: {python3:?}"
    );
    let sid = mgr
        .start(
            &python3,
            &[app],
            None,
            &[],
            80,
            24,
            "auto",
            "clean",
        )
        .expect("clean-isolated launch");
    assert_profile_holds(&mut mgr, &sid, 80, 24, 1, "clean isolation");
    mgr.stop(&sid).ok();
}

/// Size x env interaction: the floor size under the least capable TERM —
/// the combination most likely to break naive layouts — still holds.
#[test]
fn floor_size_under_dumb_term_holds() {
    let (_dir, app) = fixture_dir();
    let mut mgr = SessionManager::new();
    let sid = start_at(
        &mut mgr,
        &app,
        20,
        8,
        &[("TERM".into(), "dumb".into())],
    );
    assert_profile_holds(&mut mgr, &sid, 20, 8, 1, "20x8 + TERM=dumb");
    mgr.stop(&sid).ok();
}

/// Resize DURING a session moves the whole profile: the parsed geometry
/// follows every leg of the matrix and the app re-renders into it. The
/// anchored generation counter makes each leg wait for a frame drawn
/// AFTER the previous leg's audit — a stale clipped frame cannot pass.
#[test]
fn live_resize_walks_the_matrix() {
    let (_dir, app) = fixture_dir();
    let mut mgr = SessionManager::new();
    let sid = start_at(&mut mgr, &app, 80, 24, &[]);
    let mut min_gen = 1u64;
    for (cols, rows) in [(120u16, 40u16), (20u16, 8u16), (80u16, 24u16)] {
        {
            let sess = mgr.resolve_mut(Some(&sid)).unwrap();
            sess.resize(cols, rows).expect("resize");
        }
        assert_profile_holds(
            &mut mgr,
            &sid,
            cols,
            rows,
            min_gen,
            &format!("resized to {cols}x{rows}"),
        );
        // The audit's full run types keys and resizes internally; every
        // one of those consumed inputs redraws the fixture, so the next
        // leg anchors on a counter strictly beyond whatever is on screen
        // NOW.
        let sess = mgr.resolve_mut(Some(&sid)).unwrap();
        let now = sess.observe(300).expect("observe between legs");
        min_gen = screen_generation(&now).expect("generation visible between legs") + 1;
    }
    mgr.stop(&sid).ok();
}
