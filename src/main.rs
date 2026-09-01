//! hermes-tui-lab — agent-native TUI instrumentation/testing/exploration/UX harness.
//!
//! Binary entry point. Spawns the MCP server over stdio. All behavior lives in
//! the `tui_lab` library crate; `main` only wires logging + transport.

use clap::{Parser, Subcommand};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tui_lab::mcp::TuiLabServer;

#[derive(Parser)]
#[command(name = "hermes-tui-lab", about, version, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server over stdio.
    Mcp,
    /// Check system readiness.
    Doctor,
    /// Print version information.
    Version,
    /// Generate skill documentation.
    Skill,
    /// Print a persisted run's history from disk (item 74): manifest,
    /// declared-replay ledger, findings summary, graphs. Read-only —
    /// replay renders what the run recorded; it does not relaunch
    /// sessions or re-send inputs.
    Replay {
        run_id: String,
        /// Base directory the run lives under (a repo root or a runs dir).
        /// Defaults to the current directory.
        #[arg(long)]
        root: Option<String>,
        /// Print the full transaction ledger instead of a summary.
        #[arg(long)]
        full: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Mcp => {
            start_mcp().await?;
        }
        Commands::Doctor => {
            doctor();
        }
        Commands::Version => {
            println!("hermes-tui-lab {}", env!("CARGO_PKG_VERSION"));
        }
        Commands::Skill => {
            println!("{}", tui_lab::SKILL_DOC);
        }
        Commands::Replay { run_id, root, full } => {
            replay(&run_id, root.as_deref(), full)?;
        }
    }
    Ok(())
}

/// Item 74: render a persisted run's recorded history from its durable
/// directory. This is deliberately a *read* of what the run recorded —
/// the ledger (with its declared eviction window), findings, focus graph
/// summary, and artifact list — not a re-execution: replaying inputs
/// against live children is what `tui_scenario action=run` is for, and
/// pretending a dead run's transcript is a live re-drive would be the
/// exact dishonesty the run model exists to prevent.
fn replay(run_id: &str, root: Option<&str>, full: bool) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let base = std::path::PathBuf::from(root.unwrap_or("."));
    let dir = tui_lab::run::RunContext::resolve_run_dir(&base, run_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no persisted run '{}' under {} (is it under .tui-lab/runs? pass --root to point at the repo/runs directory)",
            run_id,
            base.to_string_lossy()
        )
    })?;
    let run = tui_lab::run::RunContext::restore(&dir)?;
    let manifest = tui_lab::run::manifest::load(&dir)?;
    let sessions = run
        .launch_specs()
        .into_iter()
        .map(|(sid, spec)| format!("  {} — {} {}", sid, spec.command, spec.args.join(" ")))
        .collect::<Vec<_>>()
        .join("\n");

    let _ = writeln!(out, "run {}", run.id);
    let _ = writeln!(out, "{}", "=".repeat(16 + run.id.len()));
    let _ = writeln!(
        out,
        "started: {}  closed: {}  history_complete: {}",
        chrono_like(run.started_at),
        manifest.closed,
        manifest.history_complete
    );
    if let Some(d) = run.run_dir() {
        let _ = writeln!(out, "artifacts: {}", d.to_string_lossy());
    }
    if !sessions.is_empty() {
        let _ = writeln!(out, "sessions (from manifest launch specs):");
        let _ = writeln!(out, "{}", sessions);
    }
    if !manifest.history_complete {
        let _ = writeln!(
            out,
            "NOTE: ledger evicted {} record(s) before flush; replay starts at seq {:?} — the run is NOT replay-complete",
            manifest.dropped_records, manifest.first_available_seq
        );
    }

    // The declared-replay transaction ledger.
    let txs = run.transactions();
    let _ = writeln!(
        out,
        "\ntransactions ({} in ledger, {} lifetime):",
        txs.len(),
        run.transaction_total()
    );
    if full {
        for t in txs {
            let _ = writeln!(
                out,
                "  {:>4}  +{:>4}ms  {:<14} settle={:<9} cells={:<5} {}",
                t.seq, t.elapsed_ms, t.action, t.settle, t.changed_cells, t.session
            );
        }
    } else {
        // Compact summary: per-action counts and settle outcomes.
        let mut by_action: std::collections::BTreeMap<String, (u64, u64)> =
            std::collections::BTreeMap::new(); // action -> (total, settled)
        for t in txs {
            let e = by_action.entry(t.action.clone()).or_default();
            e.0 += 1;
            if t.settled() {
                e.1 += 1;
            }
        }
        for (action, (total, settled)) in by_action {
            let _ = writeln!(out, "  {:<24} {:>4} (settled {})", action, total, settled);
        }
        let _ = writeln!(
            out,
            "  (full ledger: hermes-tui-lab replay {} --full)",
            run.id
        );
    }

    // Findings summary.
    let findings = run.findings();
    let _ = writeln!(out, "\nfindings ({}):", findings.len());
    for f in findings.iter().take(40) {
        let _ = writeln!(out, "  [{:<5}] {:<28} {}", f.severity, f.id, f.summary);
    }
    if findings.len() > 40 {
        let _ = writeln!(
            out,
            "  … and {} more (findings.json in the run dir)",
            findings.len() - 40
        );
    }

    // Graphs + artifacts.
    let _ = writeln!(
        out,
        "\nstate graph: {} states, {} transitions",
        run.state_graph.state_count(),
        run.state_graph.transition_count()
    );
    let _ = writeln!(
        out,
        "focus graph: {} controls, {} edges (tab cycle: {})",
        run.focus_graph.nodes.len(),
        run.focus_graph.edges.len(),
        run.focus_graph
            .tab_cycle()
            .map(|c| c.join("→"))
            .unwrap_or_else(|| "none".into())
    );
    let arts = run.artifacts();
    if !arts.is_empty() {
        let _ = writeln!(out, "artifacts ({}):", arts.len());
        for a in arts {
            let _ = writeln!(out, "  {:<24} {}", a.id, a.summary);
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Unix-millis → local-ish "YYYY-MM-DD HH:MM:SS" without pulling chrono:
/// civil-from-days is a well-known integer algorithm; good enough for a
/// header line, and honest (UTC) in a format that says so.
fn chrono_like(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y, mth, d, h, m, s
    )
}

async fn start_mcp() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let server = TuiLabServer::new();
    let transport = stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

fn doctor() {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "hermes-tui-lab doctor");
    let _ = writeln!(out, "======================");
    let _ = writeln!(out);

    // Every line below is a real probe, not a hardcoded claim. The doctor
    // must fail visibly when a subsystem is broken (audit: honest readiness).

    // 1. PTY spawn + observe: actually run a child through the backend.
    let pty = std::panic::catch_unwind(|| {
        use tui_lab::backend::TerminalBackend as _;
        let mut backend = tui_lab::backend::PortablePtyBackend::new(80, 24);
        backend
            .start(
                "python3",
                &["-c".to_string(), "print('doctor-ok')".to_string()],
                None,
                &[],
                80,
                24,
            )
            .is_ok()
            && {
                let _ = backend.wait(
                    tui_lab::backend::WaitCond::Text("doctor-ok".into()),
                    std::time::Duration::from_secs(5),
                );
                backend.state().is_ok()
            }
    })
    .unwrap_or(false);
    report(
        &mut out,
        "Terminal PTY",
        pty,
        "spawn python3 + parse screen",
    );

    // 2. Screen parsing / semantic model: analyze a synthetic frame.
    let semantic = std::panic::catch_unwind(|| {
        let screen = tui_lab::screen::ScreenState {
            cols: 40,
            rows: 5,
            cursor: tui_lab::screen::CursorState {
                x: 1,
                y: 1,
                visible: true,
            },
            title: None,
            cells: Vec::new(),
            viewport_text: vec![
                "[ Save ]".to_string(),
                "Host: localhost".to_string(),
                "".to_string(),
                "".to_string(),
                "".to_string(),
            ],
            scrollback: Vec::new(),
            hyperlinks: Vec::new(),
            raw_hash: String::new(),
            visual_hash: String::new(),
            structure_hash: String::new(),
            process: tui_lab::screen::ProcessState {
                running: true,
                exit_code: None,
                exit_signal: None,
                cwd: None,
                pid: None,
            },
        };
        let sem = tui_lab::semantic::analyze(&screen);
        sem.controls.iter().any(|c| c.label == "Save")
            && sem
                .controls
                .iter()
                .any(|c| c.kind == tui_lab::semantic::ControlKind::Field)
    })
    .unwrap_or(false);
    report(
        &mut out,
        "Semantic model",
        semantic,
        "control inference on synthetic frame",
    );

    // 3. Recording: construct a recorder and produce NDJSON.
    let recording = std::panic::catch_unwind(|| {
        let mut r = tui_lab::recording::AsciicastRecorder::new(80, 24, false);
        r.record_output(b"probe");
        r.event_count() > 0
    })
    .unwrap_or(false);
    report(
        &mut out,
        "Recording (asciicast)",
        recording,
        "recorder produces events",
    );

    // 4. Checkpoints: save + compare in a temp dir.
    let checkpoints = std::panic::catch_unwind(|| {
        let dir = std::env::temp_dir().join(format!("tui-lab-doctor-{}", std::process::id()));
        let mut store =
            tui_lab::checkpoint::CheckpointStore::with_run_dir(dir.to_string_lossy().to_string());
        let screen = probe_screen();
        let name = store.save("doctor", 0, None, &screen, None);
        let ok = store.contains("doctor", &name);
        let _ = std::fs::remove_dir_all(&dir);
        ok
    })
    .unwrap_or(false);
    report(
        &mut out,
        "Checkpoints",
        checkpoints,
        "save + persist round-trip",
    );

    // 5. Scenario model: build + validate.
    let scenarios = std::panic::catch_unwind(|| {
        use tui_lab::scenario::model::Scenario;
        let s = Scenario::new("doctor").act(serde_json::json!({"action":"key","key":"enter"}));
        s.is_valid() && s.step_count() == 1
    })
    .unwrap_or(false);
    report(&mut out, "Scenarios", scenarios, "model build + validate");

    // 6. Exploration: state graph bookkeeping (identity-keyed, P0 fix 3).
    let exploration = std::panic::catch_unwind(|| {
        use tui_lab::exploration::state_graph::StateIdentity;
        let mut g = tui_lab::exploration::StateGraph::new(
            tui_lab::exploration::ExplorationBudget::default(),
        );
        let a = StateIdentity::from_parts("h1");
        let b = StateIdentity::from_parts("h2");
        g.record_state_identity(&a, 0);
        g.record_state_identity(&b, 1);
        g.record_transition_identity(&a, &b, "k");
        g.state_count() == 2 && g.transition_count() == 1
    })
    .unwrap_or(false);
    report(
        &mut out,
        "Exploration (state graph)",
        exploration,
        "identity graph record + count",
    );

    // 7. Coverage: honest probe for the optional tuicov executable. Absent
    // is a warn, not a fail — the lab works without it, only the optional
    // coverage correlation degrades.
    let coverage = tui_lab::coverage::tuicov::is_available();
    tier(
        &mut out,
        "Coverage (tuicov)",
        if coverage { Tier::Ok } else { Tier::Warn },
        if coverage {
            "optional executable on PATH"
        } else {
            "optional executable not on PATH — coverage correlation unavailable"
        },
    );

    // 8. Framework probes: parse a synthetic ratatui manifest (this crate
    // itself legitimately has no TUI framework dependency — it IS the harness).
    let framework = std::panic::catch_unwind(|| {
        let dir = std::env::temp_dir().join(format!("tui-lab-doctor-fw-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let manifest = dir.join("Cargo.toml");
        let _ = std::fs::write(&manifest, "[dependencies]\nratatui = \"0.29\"\n");
        let det = tui_lab::framework::detect::detect(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        det.framework.as_deref() == Some("ratatui")
    })
    .unwrap_or(false);
    tier(
        &mut out,
        "Framework detection",
        Tier::from_ok(framework),
        "ratatui manifest parse",
    );

    // 9. python3 presence (fixtures + many audits depend on it). Missing
    // python3 degrades audits that drive python fixtures — a warn, since
    // the core PTY/semantic/recording paths run against any child.
    let python3 = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    tier(
        &mut out,
        "python3 (fixture runtime)",
        if python3 { Tier::Ok } else { Tier::Warn },
        if python3 {
            "child on PATH"
        } else {
            "python3 not on PATH — python-fixture audits unavailable"
        },
    );

    let _ = writeln!(out);
    let core = pty && semantic && recording && checkpoints && scenarios && exploration;
    if core {
        let _ = writeln!(out, "Core subsystems operational.");
    } else {
        let _ = writeln!(out, "CORE SUBSYSTEM FAILURES — see [FAIL] lines above.");
        std::process::exit(1);
    }
}

fn report(out: &mut impl std::io::Write, name: &str, ok: bool, detail: &str) {
    tier(out, name, Tier::from_ok(ok), detail);
}

/// Readiness tier for a doctor probe: only [`Tier::Fail`] (a core subsystem
/// is actually broken) affects the exit code; [`Tier::Warn`] is degraded
/// capability and [`Tier::Skip`] is an optional integration that isn't
/// present. The old single ok/FAIL bit conflated "broken" with "absent" —
/// an optional executable missing from PATH is not a failure of the lab.
enum Tier {
    Ok,
    /// Degraded but functional (or an optional integration absent).
    Warn,
    Fail,
}

impl Tier {
    fn from_ok(ok: bool) -> Self {
        if ok {
            Tier::Ok
        } else {
            Tier::Fail
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Tier::Ok => "[ok]",
            Tier::Warn => "[warn]",
            Tier::Fail => "[FAIL]",
        }
    }
}

fn tier(out: &mut impl std::io::Write, name: &str, t: Tier, detail: &str) {
    let _ = writeln!(
        out,
        "{:<28} {}  ({})",
        format!("{}:", name),
        t.label(),
        detail
    );
}

/// Minimal synthetic screen for doctor probes.
fn probe_screen() -> tui_lab::screen::ScreenState {
    tui_lab::screen::ScreenState {
        cols: 10,
        rows: 3,
        cursor: tui_lab::screen::CursorState {
            x: 0,
            y: 0,
            visible: true,
        },
        title: None,
        cells: Vec::new(),
        viewport_text: vec!["a".to_string(), String::new(), String::new()],
        scrollback: Vec::new(),
        hyperlinks: Vec::new(),
        raw_hash: "r".into(),
        visual_hash: "v".into(),
        structure_hash: "s".into(),
        process: tui_lab::screen::ProcessState {
            running: true,
            exit_code: None,
            exit_signal: None,
            cwd: None,
            pid: None,
        },
    }
}
