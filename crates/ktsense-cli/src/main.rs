//! `ktsense` entry point.
//!
//! Commands are declared here in full from the start so the CLI surface, the MCP tool catalogue and
//! the documentation cannot drift apart. Each one that has not landed yet reports that it is not
//! implemented, which keeps `--help` honest instead of advertising behaviour that does not exist.

#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use ktsense_core::{render_markdown, FileSkeleton, RenderOptions};

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
    Outline {
        file: PathBuf,
        /// Include private and internal declarations, hidden by default.
        #[arg(long)]
        private: bool,
        /// Include the first line of each declaration's KDoc.
        #[arg(long)]
        kdoc: bool,
    },
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

/// Process exit codes are a contract for callers and future subcommands, so they are named once
/// here rather than materialised as scattered `std::process::exit` calls. `symbols` and `trace`
/// will end with [`Exit::Ambiguous`] when a name resolves to several candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Success,
    Failure,
    #[allow(dead_code)]
    Ambiguous,
    Unimplemented,
}

impl Exit {
    fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Failure => 1,
            Exit::Ambiguous => 2,
            Exit::Unimplemented => 70,
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit.code())
    }
}

/// A command that could not complete: the message the user sees and the exit code it ends with,
/// kept together so no code path decides an exit code on its own.
#[derive(Debug)]
struct CommandError {
    exit: Exit,
    message: String,
}

impl CommandError {
    fn read(file: &Path, error: &io::Error) -> Self {
        let path = file.display();
        let message = match error.kind() {
            io::ErrorKind::NotFound => format!("ktsense: no such file: {path}"),
            _ => format!("ktsense: cannot read {path}: {error}"),
        };
        Self {
            exit: Exit::Failure,
            message,
        }
    }

    fn unparseable(file: &Path) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!(
                "ktsense: {} has Kotlin syntax errors and cannot be outlined",
                file.display()
            ),
        }
    }

    fn extraction(file: &Path, error: &anyhow::Error) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: cannot outline {}: {error}", file.display()),
        }
    }

    fn serialization(error: serde_json::Error) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: cannot serialize outline as JSON: {error}"),
        }
    }

    fn unimplemented(card: &str) -> Self {
        Self {
            exit: Exit::Unimplemented,
            message: format!("ktsense: {card} is not implemented yet"),
        }
    }
}

fn main() -> ExitCode {
    init_tracing();
    match run(Cli::parse()) {
        Ok(output) => {
            print!("{output}");
            Exit::Success.into()
        }
        Err(failure) => {
            eprintln!("{}", failure.message);
            failure.exit.into()
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KTSENSE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> Result<String, CommandError> {
    let format = cli.format;
    match cli.command {
        Command::Outline {
            file,
            private,
            kdoc,
        } => {
            let mut options = RenderOptions::default();
            if private {
                options = options.with_private();
            }
            if kdoc {
                options = options.with_doc();
            }
            outline(&file, format, &options)
        }
        ref pending => Err(CommandError::unimplemented(not_implemented_label(pending))),
    }
}

fn outline(file: &Path, format: Format, options: &RenderOptions) -> Result<String, CommandError> {
    let source = fs::read_to_string(file).map_err(|error| CommandError::read(file, &error))?;
    reject_syntax_errors(file, &source)?;
    let skeleton = ktsense_syntax::extract(file.to_string_lossy(), &source)
        .map_err(|error| CommandError::extraction(file, &error))?;
    present(&skeleton, format, options)
}

// tree-sitter is error-tolerant and returns a tree for broken input, so `extract` alone would
// happily outline garbage. Gating on `has_error` keeps a syntactically invalid file from being
// presented as a confident skeleton, which the accuracy-honesty rule forbids.
fn reject_syntax_errors(file: &Path, source: &str) -> Result<(), CommandError> {
    let tree =
        ktsense_syntax::parse(source).map_err(|error| CommandError::extraction(file, &error))?;
    if tree.root_node().has_error() {
        return Err(CommandError::unparseable(file));
    }
    Ok(())
}

fn present(
    skeleton: &FileSkeleton,
    format: Format,
    options: &RenderOptions,
) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_markdown(skeleton, options)),
        Format::Json => serde_json::to_string_pretty(skeleton)
            .map(|json| format!("{json}\n"))
            .map_err(CommandError::serialization),
    }
}

fn not_implemented_label(command: &Command) -> &'static str {
    match command {
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
    }
}
