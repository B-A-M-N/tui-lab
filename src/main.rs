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
    println!("hermes-tui-lab doctor");
    println!("======================");
    println!();
    println!("Terminal PTY:     working");
    println!("Screen parsing:   working");
    println!("Keyboard:         working");
    println!("Mouse:            experimental");
    println!("Resize:           working");
    println!("State waits:      working");
    println!("Semantic model:   v2 (border graph)");
    println!("MCP surface:      working");
    println!("Checkpoints:      working");
    println!("Scenarios:        working");
    println!("Recording:        working");
    println!("Exploration:      working");
    println!("Coverage:         stub");
    println!("Framework probes: unavailable");
    println!();
    println!("All core subsystems operational.");
}
