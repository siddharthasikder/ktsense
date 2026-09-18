//! `ktsense` entry point.
//!
//! Commands are declared here in full from the start so the CLI surface, the MCP tool catalogue and
//! the documentation cannot drift apart. Each one reports that it is not implemented yet until its
//! card lands, which keeps `--help` honest instead of advertising behaviour that does not exist.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "ktsense",
    version,
    about = "Agent-first Kotlin code understanding: compressed outlines, symbol tracing, and an MCP server",
    long_about = None
)]
struct Cli {
    /// Workspace root. Defaults to the nearest enclosing git repository, else the current directory.
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Output format. Markdown is compressed for prompt injection; JSON is for tool chaining.
    #[arg(long, global = true, value_enum, default_value_t = Format::Md)]
    format: Format,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Md,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compressed declaration skeleton of one file
    Outline { file: PathBuf },
    /// Find declarations by name across the workspace
    Symbols { query: String },
    /// Definition, usages, implementors and callers of one symbol
    Trace { symbol: String },
    /// Import graph of the workspace
    Deps,
    /// Token-budgeted map of the most central files
    Map {
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },
    /// Syntax-check files; exits non-zero when a file has errors
    Check { path: PathBuf },
    /// Budgeted context bundle for one symbol
    Context {
        symbol: String,
        #[arg(long, default_value_t = 2000)]
        budget: usize,
    },
    /// Index phase, counts and engine version
    Status,
    /// Run the MCP stdio server
    Mcp,
    /// Manage the warm-session daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    Start,
    Stop,
    Status,
}

/// Exit code for a command that exists but is not implemented yet.
const EXIT_UNIMPLEMENTED: u8 = 70;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KTSENSE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let pending = match &cli.command {
        Command::Outline { .. } => "outline (KT-07)",
        Command::Symbols { .. } => "symbols (KT-16)",
        Command::Trace { .. } => "trace (KT-18)",
        Command::Deps => "deps (KT-20)",
        Command::Map { .. } => "map (KT-22)",
        Command::Check { .. } => "check (KT-23)",
        Command::Context { .. } => "context (KT-35)",
        Command::Status => "status (KT-36)",
        Command::Mcp => "mcp (KT-31)",
        Command::Daemon { .. } => "daemon (KT-27)",
    };

    eprintln!("ktsense: {pending} is not implemented yet");
    ExitCode::from(EXIT_UNIMPLEMENTED)
}
