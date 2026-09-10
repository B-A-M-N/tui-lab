//! tui-lab — agent-native TUI instrumentation/testing/exploration/UX harness.
//!
//! Binary entry point. Spawns the MCP server over stdio. All behavior lives in
//! the `tui_lab` library crate; `main` only wires logging + transport.

use clap::{Parser, Subcommand};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tui_lab::mcp::TuiLabServer;

#[derive(Parser)]
#[command(name = "tui-lab", about, version, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server over stdio.
    Mcp,
    /// Check system readiness.
    Doctor {
        /// Emit the readiness matrix as JSON (P1.10): machine-consumable,
        /// same probes, stable field names.
        #[arg(long)]
        json: bool,
    },
    /// Print version information.
    Version,
    /// Generate skill documentation.
    Skill {
        /// Splice the generated Tools and Resources sections into a
        /// SKILL.md on disk (between their headings, preserving all
        /// other prose) instead of printing. This is the fix loop for
        /// the drift the registry parity test catches.
        #[arg(long)]
        write: bool,
        /// skill --write: explicit output path. `CARGO_MANIFEST_DIR` is
        /// a developer-tree concept — after `cargo install`, rewriting
        /// the crate's manifest directory is not a sensible public
        /// runtime behavior. With `--path` the file is read and written
        /// where the caller says; without it, --write falls back to the
        /// manifest-dir SKILL.md (the developer/build loop, unchanged).
        #[arg(long)]
        path: Option<String>,
        /// Splice the generated per-tool selector vocabulary into
        /// README.md (P1.9) instead of the SKILL.md sections — the
        /// README's `**Actions:**`-style tables, regenerated from the
        /// same registry. Honors `--path` the same way.
        #[arg(long)]
        write_readme: bool,
    },
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
    /// Run a saved scenario against a real session, reusing the canonical
    /// kernel (the same runner/evidence path MCP uses). Machine output JSON.
    Scenario {
        /// Scenario id or unambiguous name.
        #[arg(long = "scenario")]
        scenario: String,
        /// Command to launch as the replay target (the scenario currently
        /// requires `inherit_session=true`; this CLI starts the supplied
        /// target and drives it).
        #[arg(long = "command")]
        command: String,
        /// Arguments for the command.
        #[arg(last = true)]
        args: Vec<String>,
        /// Number of repeat runs for flakiness classification.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Failure policy override: stop or continue.
        #[arg(long)]
        on_failure: Option<String>,
        /// Sensitive parameter value `NAME=value`; repeatable.
        #[arg(long = "param")]
        params: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Mcp => {
            start_mcp().await?;
        }
        Commands::Doctor { json } => {
            doctor(json).await?;
        }
        Commands::Version => {
            println!("tui-lab {}", env!("CARGO_PKG_VERSION"));
        }
        Commands::Skill {
            write,
            path,
            write_readme,
        } => {
            if write_readme {
                readme_write(path)?;
            } else if write {
                skill_write(path)?;
            } else {
                println!("{}", tui_lab::SKILL_DOC);
            }
        }
        Commands::Replay { run_id, root, full } => {
            replay(&run_id, root.as_deref(), full)?;
        }
        Commands::Scenario {
            scenario,
            command,
            args,
            repeat,
            on_failure,
            params,
        } => {
            scenario_cli(
                &scenario,
                &command,
                &args,
                repeat,
                on_failure.as_deref(),
                &params,
            )
            .await?;
        }
    }
    Ok(())
}

/// The CI-facing scenario runner over the exact library kernel. It starts a
/// real session, loads the scenario from the requested run, replays through
/// [`tui_lab::scenario::runner::ScenarioRunner`], and prints stable JSON.
async fn scenario_cli(
    scenario_key: &str,
    command: &str,
    args: &[String],
    repeat: u32,
    on_failure: Option<&str>,
    params: &[String],
) -> anyhow::Result<()> {
    use tui_lab::scenario::model::FailurePolicy;
    use tui_lab::scenario::runner::{FlakinessVerdict, ScenarioRunner};

    let policy_override = match on_failure {
        None => None,
        Some("stop") => Some(FailurePolicy::Stop),
        Some("continue") => Some(FailurePolicy::Continue),
        Some(other) => anyhow::bail!("unknown on_failure '{other}' (expected stop|continue)"),
    };
    let mut parameter_values = Vec::new();
    for raw in params {
        let (name, value) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid --param '{raw}' (expected NAME=value)"))?;
        parameter_values.push(tui_lab::scenario::model::ParameterValue {
            name: name.to_string(),
            value: value.to_string(),
        });
    }

    let pool = tui_lab::session::SessionPool::new();
    let sid = pool
        .start(command, args, None, &[], 80, 24, "auto", "local")
        .await?;
    // The run context is loaded from the current directory's runs root.
    // This CLI intentionally requires an explicit persisted run (scenario
    // files are run-scoped evidence, not global names).
    let scenario = {
        let mut loaded = None;
        let cwd = std::env::current_dir()?;
        let mut entries = std::fs::read_dir(cwd.join(".tui-lab/runs"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect::<Vec<_>>();
        entries.sort();
        for dir in entries.into_iter().rev() {
            if !dir.is_dir() {
                continue;
            }
            let Ok(run) = tui_lab::run::RunContext::restore(&dir) else {
                continue;
            };
            if let Ok(sc) = run.load_scenario(scenario_key) {
                loaded = Some(sc);
                break;
            }
        }
        loaded.ok_or_else(|| {
            anyhow::anyhow!("scenario '{scenario_key}' not found under ./.tui-lab/runs")
        })?
    };

    let report = {
        let run = std::sync::Arc::new(std::sync::Mutex::new(tui_lab::run::RunContext::ephemeral()));
        pool.with_session(Some(&sid), move |sess| {
            ScenarioRunner::run_repeat(
                &scenario,
                sess,
                &parameter_values,
                Some(&run),
                policy_override,
                repeat,
            )
        })
        .await?
    };
    let verdict = report.verdict.name();
    let passed = report.verdict == FlakinessVerdict::StablePass;
    let json = serde_json::json!({
        "scenario": scenario_key,
        "session": sid,
        "repeat": report.repeat,
        "passed_runs": report.passed_runs,
        "failed_runs": report.failed_runs,
        "pass_rate_pct": report.pass_rate_pct,
        "verdict": verdict,
        "passed": passed,
        "first_run": report.first_run,
        "last_run": report.last_run,
    });
    println!("{}", serde_json::to_string_pretty(&json)?);
    if !passed {
        anyhow::bail!("scenario replay did not stably pass");
    }
    pool.stop(&sid).await.ok();
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

    let _ = writeln!(out, "run {}", run.id());
    let _ = writeln!(out, "{}", "=".repeat(16 + run.id().len()));
    let _ = writeln!(
        out,
        "started: {}  closed: {}  history_complete: {}",
        chrono_like(run.started_at()),
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
        let _ = writeln!(out, "  (full ledger: tui-lab replay {} --full)", run.id(),);
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
        run.graphs().state_graph.state_count(),
        run.graphs().state_graph.transition_count()
    );
    let _ = writeln!(
        out,
        "focus graph: {} controls, {} edges (tab cycle: {})",
        run.graphs().focus_graph.nodes.len(),
        run.graphs().focus_graph.edges.len(),
        run.graphs()
            .focus_graph
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

/// `skill --write`: regenerate the generated sections into a SKILL.md on
/// disk. P2: an explicit `--path` is the public contract — the default
/// (CARGO_MANIFEST_DIR/SKILL.md) is the developer/build loop, available
/// only when the manifest directory actually exists as such.
fn skill_write(path: Option<String>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("SKILL.md"),
    };
    let doc = std::fs::read_to_string(&path)?;
    let spliced = tui_lab::mcp::registry::splice_skill_sections(&doc).ok_or_else(|| {
        anyhow::anyhow!("{} is missing a generated section heading", path.display())
    })?;
    if spliced == doc {
        eprintln!("{} already current — nothing to write.", path.display());
        return Ok(());
    }
    std::fs::write(&path, spliced)?;
    eprintln!(
        "{} updated: Tools and Resources sections regenerated from the registry.",
        path.display()
    );
    Ok(())
}

/// `skill --write-readme` (P1.9): regenerate the README's per-tool
/// selector vocabulary from the registry. Same `--path` contract as
/// `skill --write`.
fn readme_write(path: Option<String>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md"),
    };
    let doc = std::fs::read_to_string(&path)?;
    let spliced = tui_lab::mcp::registry::splice_readme_selectors(&doc).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is missing the generated-selector markers — add the \
             <!-- BEGIN/END GENERATED SELECTORS --> block first",
            path.display()
        )
    })?;
    if spliced == doc {
        eprintln!("{} already current — nothing to write.", path.display());
        return Ok(());
    }
    std::fs::write(&path, spliced)?;
    eprintln!(
        "{} updated: selector vocabulary regenerated from the registry.",
        path.display()
    );
    Ok(())
}

async fn doctor(json_out: bool) -> anyhow::Result<()> {
    // P1.10: probe data first, render second — the same matrix feeds the
    // human text and `--json`. Subsystem probes answer "do the pieces
    // work"; product probes answer "can an agent actually USE this
    // end-to-end on this machine".
    use tui_lab::diagnostic::product_probes as pp;
    let mut entries: Vec<ProbeEntry> = Vec::new();

    // ── subsystem probes ──

    // PTY spawn + observe: actually run a child through the backend.
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
    entries.push(ProbeEntry::tier(
        "Terminal PTY",
        Tier::from_ok(pty),
        "spawn python3 + parse screen",
    ));

    // Screen parsing / semantic model: analyze a synthetic frame.
    let semantic = std::panic::catch_unwind(|| {
        let screen = probe_screen();
        let mut screen = screen;
        screen.cols = 40;
        screen.rows = 5;
        screen.viewport_text = vec![
            "[ Save ]".to_string(),
            "Host: localhost".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let sem = tui_lab::semantic::analyze(&screen);
        sem.controls.iter().any(|c| c.label == "Save")
            && sem
                .controls
                .iter()
                .any(|c| c.kind == tui_lab::semantic::ControlKind::Field)
    })
    .unwrap_or(false);
    entries.push(ProbeEntry::tier(
        "Semantic model",
        Tier::from_ok(semantic),
        "control inference on synthetic frame",
    ));

    // Recording: construct a recorder and produce NDJSON.
    let recording = std::panic::catch_unwind(|| {
        let mut r = tui_lab::recording::AsciicastRecorder::new(80, 24, false);
        r.record_output(b"probe");
        r.event_count() > 0
    })
    .unwrap_or(false);
    entries.push(ProbeEntry::tier(
        "Recording (asciicast)",
        Tier::from_ok(recording),
        "recorder produces events",
    ));

    // Checkpoints: save + compare in a temp dir.
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
    entries.push(ProbeEntry::tier(
        "Checkpoints",
        Tier::from_ok(checkpoints),
        "save + persist round-trip",
    ));

    // Scenario model: build + validate.
    let scenarios = std::panic::catch_unwind(|| {
        use tui_lab::scenario::model::Scenario;
        let s = Scenario::new("doctor").act(serde_json::json!({"action":"key","key":"enter"}));
        s.is_valid() && s.step_count() == 1
    })
    .unwrap_or(false);
    entries.push(ProbeEntry::tier(
        "Scenarios (model)",
        Tier::from_ok(scenarios),
        "model build + validate",
    ));

    // Exploration: state graph bookkeeping (identity-keyed, P0 fix 3).
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
    entries.push(ProbeEntry::tier(
        "Exploration (state graph)",
        Tier::from_ok(exploration),
        "identity graph record + count",
    ));

    // Coverage: honest probe for the optional tuicov executable. Absent
    // is a warn, not a fail — the lab works without it, only the optional
    // coverage correlation degrades.
    let coverage = tui_lab::coverage::tuicov::is_available();
    entries.push(ProbeEntry::tier(
        "Coverage (tuicov)",
        if coverage { Tier::Ok } else { Tier::Warn },
        if coverage {
            "optional executable on PATH"
        } else {
            "optional executable not on PATH — coverage correlation unavailable"
        },
    ));

    // Framework probes: parse a synthetic ratatui manifest (this crate
    // itself legitimately has no TUI framework dependency — it IS the harness).
    let framework = std::panic::catch_unwind(|| {
        let dir = std::env::temp_dir().join(format!("tui-lab-doctor-fw-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let manifest = dir.join("Cargo.toml");
        let _ = std::fs::write(&manifest, "[dependencies]\nratatui = \"0.29\"\n");
        let det = tui_lab::framework::detect::detect(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        det.primary.as_ref().map(|c| c.name.as_str()) == Some("ratatui")
    })
    .unwrap_or(false);
    entries.push(ProbeEntry::tier(
        "Framework detection",
        Tier::from_ok(framework),
        "ratatui manifest parse",
    ));

    // python3 presence (fixtures + many audits depend on it). Missing
    // python3 degrades audits that drive python fixtures — a warn, since
    // the core PTY/semantic/recording paths run against any child.
    let python3 = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    entries.push(ProbeEntry::tier(
        "python3 (fixture runtime)",
        if python3 { Tier::Ok } else { Tier::Warn },
        if python3 {
            "child on PATH"
        } else {
            "python3 not on PATH — python-fixture audits unavailable"
        },
    ));

    // Native cooperation, end-to-end (doctor item 38): launch the shipped
    // cooperative fixture and prove the whole side-channel path.
    let native_probe = if python3 {
        std::panic::catch_unwind(tui_lab::diagnostic::native_cooperation_probe).unwrap_or_else(
            |_| tui_lab::diagnostic::NativeCooperationReport {
                ran: false,
                frames_received: 0,
                frames_invalid: 0,
                native_control_resolved: false,
                native_focus_applied: false,
                detail: "probe panicked".to_string(),
            },
        )
    } else {
        tui_lab::diagnostic::NativeCooperationReport {
            ran: true,
            frames_received: 1,
            frames_invalid: 0,
            native_control_resolved: true,
            native_focus_applied: true,
            detail: "python3 unavailable — cooperation probe skipped".to_string(),
        }
    };
    let native_ok = native_probe.ran
        && native_probe.frames_received > 0
        && native_probe.frames_invalid == 0
        && native_probe.native_control_resolved
        && native_probe.native_focus_applied;
    entries.push(ProbeEntry::tier(
        "Native cooperation (TUI_LAB_SEMANTIC)",
        if python3 {
            Tier::from_ok(native_ok)
        } else {
            Tier::Warn
        },
        &native_probe.detail,
    ));

    // ── product probes (P1.10): the end-to-end matrix ──
    let p = pp::mcp_stdio_roundtrip().await;
    entries.push(ProbeEntry::raw(&p));
    let p = pp::pty_inspect_act_diff().await;
    entries.push(ProbeEntry::raw(&p));
    let p = pp::scenario_record_replay().await;
    entries.push(ProbeEntry::raw(&p));
    let p = pp::contract_static_check().await;
    entries.push(ProbeEntry::raw(&p));
    let p = pp::workflow_diagnostic().await;
    entries.push(ProbeEntry::raw(&p));
    let p = pp::persistence_roundtrip().await;
    entries.push(ProbeEntry::raw(&p));
    entries.push(ProbeEntry::raw(&pp::tmux_available()));
    entries.push(ProbeEntry::raw(&pp::strict_isolation()));

    // ── render ──
    if json_out {
        let matrix = serde_json::json!({
            "tool": "tui-lab",
            "command": "doctor",
            "ok": entries.iter().all(|e| e.tier != "FAIL"),
            "probes": entries.iter().map(|e| serde_json::json!({
                "name": e.name,
                "tier": e.tier.to_lowercase(),
                "detail": e.detail,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&matrix)?);
    } else {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "tui-lab doctor");
        let _ = writeln!(out, "======================");
        let _ = writeln!(out);
        for e in &entries {
            let t = match e.tier {
                "ok" => Tier::Ok,
                "warn" => Tier::Warn,
                _ => Tier::Fail,
            };
            tier(&mut out, &e.name, t, &e.detail);
        }
        let _ = writeln!(out);
        let core_failed = entries.iter().any(|e| e.tier == "FAIL");
        if core_failed {
            let _ = writeln!(out, "CORE SUBSYSTEM FAILURES — see [FAIL] lines above.");
            std::process::exit(1);
        }
        let degraded = entries.iter().filter(|e| e.tier == "warn").count();
        if degraded > 0 {
            let _ = writeln!(
                out,
                "Operational ({degraded} degraded capability — see [warn] lines)."
            );
        } else {
            let _ = writeln!(out, "Fully operational.");
        }
    }
    Ok(())
}

/// Readiness tier for a doctor probe: only Fail (a core subsystem is
/// actually broken) affects the exit code; Warn is degraded capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        "{:<42} {}  ({})",
        format!("{}:", name),
        t.label(),
        detail
    );
}

/// One row of the readiness matrix (P1.10): tier as data so `--json` and
/// the text render share one source.
struct ProbeEntry {
    name: String,
    /// "ok" | "warn" | "FAIL"
    tier: &'static str,
    detail: String,
}

impl ProbeEntry {
    fn tier(name: &str, t: Tier, detail: &str) -> Self {
        ProbeEntry {
            name: name.to_string(),
            tier: t.as_tier_str(),
            detail: detail.to_string(),
        }
    }
    fn raw(p: &tui_lab::diagnostic::product_probes::ProductProbe) -> Self {
        ProbeEntry {
            name: p.name.to_string(),
            tier: match p.tier {
                "ok" => "ok",
                "warn" => "warn",
                _ => "FAIL",
            },
            detail: p.detail.clone(),
        }
    }
}

impl Tier {
    fn as_tier_str(&self) -> &'static str {
        match self {
            Tier::Ok => "ok",
            Tier::Warn => "warn",
            Tier::Fail => "FAIL",
        }
    }
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
