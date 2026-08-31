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
    /// Play back a previous run.
    Replay { run_id: String },
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
        Commands::Replay { run_id } => {
            println!("Replay not yet implemented for run: {}", run_id);
        }
    }
    Ok(())
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

    // 7. Coverage: honest probe for the optional tuicov executable.
    let coverage = tui_lab::coverage::tuicov::is_available();
    report(
        &mut out,
        "Coverage (tuicov)",
        coverage,
        "optional executable on PATH",
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
    report(
        &mut out,
        "Framework detection",
        framework,
        "ratatui manifest parse",
    );

    // 9. python3 presence (fixtures + many audits depend on it).
    let python3 = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    report(
        &mut out,
        "python3 (fixture runtime)",
        python3,
        "child on PATH",
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
    let _ = writeln!(
        out,
        "{:<28} {}  ({})",
        format!("{}:", name),
        if ok { "[ok]" } else { "[FAIL]" },
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
