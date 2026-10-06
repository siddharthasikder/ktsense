//! `ktsense` entry point.
//!
//! Commands are declared here in full from the start so the CLI surface, the MCP tool catalogue and
//! the documentation cannot drift apart. Each one that has not landed yet reports that it is not
//! implemented, which keeps `--help` honest instead of advertising behaviour that does not exist.

#![forbid(unsafe_code)]

mod cache;
mod context;
mod daemon;
mod grep;
mod identifiers;
mod implementors;
mod routing;
mod status;
mod symbols;
mod text_refs;
mod trace;

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use ktsense_core::{
    build_import_graph, build_repo_map, fence_for, neutralize, render_deps_dot,
    render_deps_markdown, render_map_markdown, render_markdown, ByteRatioEstimator,
    ContextSections, DepLevel, FileSkeleton, FocusSpec, ImportGraph, NameMatcher, RenderOptions,
    RepoMap, RepoMapInput,
};
use ktsense_lsp::{CheckReport, DiagnoseReport, LspError, PassthroughError, Severity};
use regex::Regex;

#[derive(Debug, Parser)]
#[command(
    name = "ktsense",
    version = ktsense_lsp::KTSENSE_VERSION,
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
    /// Graphviz DOT. Only `deps` produces it; other commands reject it rather than pretend.
    Dot,
}

/// Whether `deps` connects packages or individual files.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Level {
    File,
    Package,
}

impl From<Level> for DepLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::File => DepLevel::File,
            Level::Package => DepLevel::Package,
        }
    }
}

/// The declaration kinds `symbols --kind` filters on. Each maps to the label the enriched row
/// carries, so the filter compares against exactly what is printed.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum KindFilter {
    Class,
    Interface,
    Object,
    Fun,
    Val,
    Var,
    Typealias,
    Constructor,
}

impl KindFilter {
    fn label(self) -> &'static str {
        match self {
            KindFilter::Class => "class",
            KindFilter::Interface => "interface",
            KindFilter::Object => "object",
            KindFilter::Fun => "fun",
            KindFilter::Val => "val",
            KindFilter::Var => "var",
            KindFilter::Typealias => "typealias",
            KindFilter::Constructor => "constructor",
        }
    }
}

/// The optional `context` sections `--only` can restrict the bundle to. The declaration line is
/// always present and so is not listed here; everything else is a section a caller can single out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ContextOnly {
    Source,
    Callers,
    Implementors,
    Outline,
}

/// Resolves the `--only` list into the section switches core budgets on. An empty list means no
/// filter was given, which is every section on, so an unfiltered `context` is byte-identical to the
/// one that shipped before the flag.
fn context_sections(only: &[ContextOnly]) -> ContextSections {
    if only.is_empty() {
        return ContextSections::all();
    }
    ContextSections {
        source: only.contains(&ContextOnly::Source),
        outline: only.contains(&ContextOnly::Outline),
        callers: only.contains(&ContextOnly::Callers),
        implementors: only.contains(&ContextOnly::Implementors),
    }
}

/// The positional query, or the pick's last dot segment when only `--pick` was given (KT-119).
/// `symbols`, `trace` and `context` all make their positional optional and require it or `--pick`,
/// so clap guarantees one is present; this derives the engine lookup name from whichever it was. A
/// pick like `InventoryItemsRepository.createProduct` yields `createProduct`, which the whole pick
/// then filters as a dot-suffix exactly as a dotted positional would.
fn query_or_pick(query: Option<String>, pick: Option<&str>) -> String {
    query.unwrap_or_else(|| {
        ktsense_core::last_segment(pick.expect("clap requires a query or --pick")).to_string()
    })
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
        /// Show declaration annotations, hidden by default; a default outline says how many it hid.
        #[arg(long)]
        annotations: bool,
    },
    /// Find declarations by name across the workspace
    Symbols {
        /// Declaration name to find; a dotted Type.member, such as OrderRepository.save, resolves
        /// its last segment and keeps only the matches the whole name is a suffix of. Optional when
        /// --pick is given, in which case the pick's last dot segment is the name.
        #[arg(required_unless_present = "pick")]
        query: Option<String>,
        /// Keep only declarations of this kind.
        #[arg(long, value_enum)]
        kind: Option<KindFilter>,
        /// Show at most this many rows when the name is ambiguous; the rest are summarized.
        #[arg(long)]
        limit: Option<usize>,
        /// Select one candidate by full FQN or a unique dot-boundary suffix of one, such as
        /// InventoryItemsRepository.createProduct, and exit successfully.
        #[arg(long, value_name = "FQN_OR_SUFFIX")]
        pick: Option<String>,
        /// List declarations whose simple name contains the query, from the syntax index, instead
        /// of matching the name exactly through the engine.
        #[arg(long)]
        contains: bool,
    },
    /// Search Kotlin source text with a regex, each hit attributed to its declaration
    Grep {
        /// Regular expression to search for. Several concepts are an alternation: `save|OrderId`.
        #[arg(allow_hyphen_values = true)]
        pattern: String,
        /// Restrict to files whose workspace-relative path starts with this prefix.
        #[arg(long, value_name = "PREFIX")]
        path: Option<String>,
        /// Search only test sources (by the source-set and filename conventions).
        #[arg(long, conflicts_with = "no_tests")]
        tests: bool,
        /// Search only production sources.
        #[arg(long)]
        no_tests: bool,
        /// Show at most this many hits per file; the rest are counted.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Definition, usages, implementors and callers of one symbol
    Trace {
        /// Symbol to trace; a dotted Type.member, such as OrderRepository.save, resolves its last
        /// segment and keeps only the match the whole name is a suffix of. Optional when --pick is
        /// given, in which case the pick's last dot segment is the name.
        #[arg(required_unless_present = "pick")]
        symbol: Option<String>,
        /// Select one candidate by full FQN or a unique dot-boundary suffix of one when the name
        /// is ambiguous.
        #[arg(long, value_name = "FQN_OR_SUFFIX")]
        pick: Option<String>,
        /// How many levels of callers to follow: 1 is the declarations that refer to the symbol.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=5))]
        depth: u8,
        /// Show at most this many reference sites per file; the rest are counted.
        #[arg(long)]
        limit: Option<usize>,
        /// Wait for the index to finish instead of answering after the 3 second cap, so the answer
        /// is always marked complete.
        #[arg(long)]
        wait_index: bool,
    },
    /// Import graph of the workspace
    Deps {
        /// Whether nodes are packages or individual files.
        #[arg(long, value_enum, default_value_t = Level::Package)]
        level: Level,
    },
    /// Token-budgeted map of the most central files
    Map {
        #[arg(long, default_value_t = 4000)]
        budget: usize,
        /// List each file's public declarations as kind and name only, no signatures, so the same
        /// budget covers more of the module.
        #[arg(long)]
        compact: bool,
        /// List declarations whose name matches this regex first, grouped by file, and stop there
        /// unless --fill spends what budget remains on the ranked map. Requires --compact.
        #[arg(long, value_name = "REGEX", requires = "compact")]
        focus: Option<String>,
        /// With --focus, spend what budget the focus section leaves on the ranked map, as a map
        /// without a focus does. Without it a focused map renders the focus section and stops.
        /// Requires --focus.
        #[arg(long, requires = "focus")]
        fill: bool,
        /// Map only files whose workspace-relative path contains this substring. Repeatable; a file
        /// is kept if it matches any (OR). Composes with --compact, --focus and --fill.
        #[arg(long, value_name = "SUBSTRING")]
        path: Vec<String>,
    },
    /// Syntax-check files; exits non-zero when a file has errors
    Check { path: PathBuf },
    /// Semantic diagnostics on one file (requires the engine index)
    Diagnose { file: PathBuf },
    /// Budgeted context bundle for one symbol
    Context {
        /// Symbol to explain; a dotted Type.member, such as OrderRepository.save, resolves its last
        /// segment and keeps only the match the whole name is a suffix of. Optional when --pick is
        /// given, in which case the pick's last dot segment is the name.
        #[arg(required_unless_present = "pick")]
        symbol: Option<String>,
        /// Select one candidate by full FQN or a unique dot-boundary suffix of one when the name
        /// is ambiguous.
        #[arg(long, value_name = "FQN_OR_SUFFIX")]
        pick: Option<String>,
        #[arg(long, default_value_t = 2000)]
        budget: usize,
        /// Render only these comma-separated sections (source, callers, implementors, outline)
        /// and spend the budget on them alone; the declaration line is always shown. An
        /// annotation's annotated declarations belong to callers. Omit for the full bundle.
        #[arg(long, value_enum, value_delimiter = ',')]
        only: Vec<ContextOnly>,
        /// Show only the Source lines matching this regular expression, plus --around context
        /// lines, each with its line number and `...` where lines were skipped.
        #[arg(long = "match", value_name = "REGEX")]
        match_pattern: Option<String>,
        /// Context lines to keep on each side of a --match hit.
        #[arg(long, default_value_t = 1)]
        around: usize,
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
    /// Serve until stopped, idle, or faulted. Hidden because it is how `start` detaches, not a
    /// command to run by hand.
    #[command(hide = true)]
    Serve,
}

/// Process exit codes are a contract the calling agent branches on, so the whole set is named once
/// here rather than left as scattered `std::process::exit` calls or clap's implicit defaults. This
/// enum is the single source the README, the agent skill file and the MCP tool descriptions quote.
///
/// - `0` success
/// - `1` the operation failed on its input: an unreadable path, a directory, or a broken Kotlin file
/// - `2` the invocation itself was malformed; clap reports these, so `2` stays reserved for usage
/// - `3` a symbol name resolved to several candidates; the caller should pick one and retry
/// - `70` the subcommand exists in the surface but has not shipped yet
///
/// `Ambiguous` is `3`, not `2`, because clap already exits `2` for its own usage errors. An agent
/// must be able to tell "I called the tool wrong" (fix the invocation) from "the name was
/// ambiguous" (choose a candidate), and those demand opposite responses.
///
/// `70` has no caller since KT-35 landed `context`, the last command the surface advertised without
/// implementing. The code stays in the table because the README, the agent skill file and the MCP
/// tool descriptions quote the whole set, and a surface-first command would need it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Success,
    Failure,
    Usage,
    Ambiguous,
    #[allow(dead_code)]
    Unimplemented,
}

impl Exit {
    fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Failure => 1,
            Exit::Usage => 2,
            Exit::Ambiguous => 3,
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
        let message = if has_kotlin_extension(file) {
            format!(
                "ktsense: {} has Kotlin syntax errors and cannot be outlined",
                file.display()
            )
        } else {
            format!(
                "ktsense: {} is not a Kotlin file (expected .kt or .kts) and could not be parsed",
                file.display()
            )
        };
        Self {
            exit: Exit::Failure,
            message,
        }
    }

    fn serialization(error: serde_json::Error) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: cannot serialize outline as JSON: {error}"),
        }
    }

    fn unsupported_format(command: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {command} does not support --format dot (only deps does)"),
        }
    }

    /// The `--match` value did not compile as a regex, so the invocation itself was malformed.
    fn bad_match_pattern(error: regex::Error) -> Self {
        Self {
            exit: Exit::Usage,
            message: format!("ktsense: --match is not a valid regular expression: {error}"),
        }
    }

    /// The `--focus` value did not compile as a regex, so the invocation itself was malformed.
    fn bad_focus_pattern(error: regex::Error) -> Self {
        Self {
            exit: Exit::Usage,
            message: format!("ktsense: --focus is not a valid regular expression: {error}"),
        }
    }

    fn passthrough(error: &PassthroughError) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {error}"),
        }
    }

    /// A daemon answered the routed command with this failure; the message is already the CLI's
    /// own, produced by the same code the in-process path runs.
    fn routed_failure(message: String) -> Self {
        Self {
            exit: Exit::Failure,
            message,
        }
    }

    fn no_daemon(root: &Path) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!(
                "ktsense: no daemon answered for {} and {} is set",
                root.display(),
                routing::REQUIRE_DAEMON_ENV
            ),
        }
    }

    fn mcp(error: anyhow::Error) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: mcp server failed: {error}"),
        }
    }

    fn engine(error: LspError) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {error}"),
        }
    }

    fn no_symbol(query: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {}", no_symbol_message(query)),
        }
    }

    /// A declaration lookup found nothing, with the KT-104 generated-aware wording: whether
    /// generated sources were searched or are absent and the build should be run.
    fn no_symbol_scoped(query: &str, root: &Path) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {}", no_symbol_message_scoped(query, root)),
        }
    }

    fn pick_missed(pick: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: no candidate has the fully-qualified name {pick}"),
        }
    }

    fn bad_pattern(pattern: &str, error: &regex::Error) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: invalid search pattern {pattern:?}: {error}"),
        }
    }
}

/// The KT-87 scope wording for a name the workspace does not declare, without the `ktsense:` prefix
/// an error line carries. Stated once so [`CommandError::no_symbol`] and the text-reference listing
/// a `trace` or `context` prints for such a name (KT-94) quote the same sentence.
fn no_symbol_message(query: &str) -> String {
    format!(
        "no declaration named {query} in this workspace; library and dependency declarations are \
         not searched, so use a text search for external types"
    )
}

/// The generated-aware not-found wording a declaration lookup prints (KT-104). When no generated
/// Kotlin sources exist under `root`, a name the lookup could not find might simply not have been
/// generated yet, so the message says so and points at the build; when generated sources do exist
/// they were searched, so the message says that instead. Shared by `symbols`, `trace` and `context`
/// so the three word a missing generated declaration the same way.
pub(crate) fn no_symbol_message_scoped(query: &str, root: &Path) -> String {
    let base = no_symbol_message(query);
    if collect_generated_kotlin_files(root).is_empty() {
        format!(
            "{base}. No generated Kotlin sources were found under build/generated, so if {query} \
             is generated, run the build and retry"
        )
    } else {
        format!("{base}. Generated Kotlin sources under build/generated were also searched")
    }
}

/// A completed command: the text to print on stdout and the status the process should end with.
/// Most commands succeed, but `check` must be able to print its report and still exit non-zero, so
/// the exit travels with the output rather than being inferred from success or failure alone. An
/// ambiguous resolution also carries a `--pick` hint for stderr, so the same instruction reaches a
/// caller watching stderr as reaches one reading the candidate list.
struct CommandOutcome {
    text: String,
    exit: Exit,
    stderr: Option<String>,
}

impl CommandOutcome {
    fn success(text: String) -> Self {
        Self {
            text,
            exit: Exit::Success,
            stderr: None,
        }
    }
}

fn main() -> ExitCode {
    init_tracing();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(usage) => return report_usage(usage),
    };
    match run(cli) {
        Ok(outcome) => {
            print!("{}", outcome.text);
            if let Some(stderr) = &outcome.stderr {
                eprintln!("{stderr}");
            }
            outcome.exit.into()
        }
        Err(failure) => {
            eprintln!("{}", failure.message);
            failure.exit.into()
        }
    }
}

// clap renders `--help` and `--version` as errors that belong on stdout with a success status, and
// genuine mistakes on stderr. Routing both through `Exit` keeps every process status defined by the
// one enum instead of letting clap call `process::exit(2)` on its own.
fn report_usage(usage: clap::Error) -> ExitCode {
    if usage.use_stderr() {
        eprint!("{usage}");
        Exit::Usage.into()
    } else {
        print!("{usage}");
        Exit::Success.into()
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

fn run(cli: Cli) -> Result<CommandOutcome, CommandError> {
    let format = cli.format;
    let base = cli.root.unwrap_or_else(|| PathBuf::from("."));
    match cli.command {
        Command::Outline {
            file,
            private,
            kdoc,
            annotations,
        } => route(
            &base,
            routing::RoutedCommand::Outline {
                file,
                private,
                kdoc,
                annotations,
            },
            format,
        ),
        Command::Deps { level } => route(
            &base,
            routing::RoutedCommand::Deps {
                level: DepLevel::from(level).into(),
            },
            format,
        ),
        Command::Symbols {
            query,
            kind,
            limit,
            pick,
            contains,
        } => symbols(
            &base,
            &query_or_pick(query, pick.as_deref()),
            kind,
            limit,
            pick.as_deref(),
            contains,
            format,
        ),
        Command::Grep {
            pattern,
            path,
            tests,
            no_tests,
            limit,
        } => grep::run(
            &base,
            &pattern,
            path.as_deref(),
            grep::TestFilter::from_flags(tests, no_tests),
            limit,
            format,
        ),
        Command::Map {
            budget,
            compact,
            focus,
            fill,
            path,
        } => route(
            &base,
            routing::RoutedCommand::Map {
                budget,
                compact,
                focus,
                fill,
                path,
            },
            format,
        ),
        Command::Trace {
            symbol,
            pick,
            depth,
            limit,
            wait_index,
        } => route(
            &base,
            routing::RoutedCommand::Trace {
                symbol: query_or_pick(symbol, pick.as_deref()),
                pick,
                depth,
                limit,
                wait_index,
            },
            format,
        ),
        Command::Daemon { action } => match action {
            DaemonAction::Start => daemon::start(&base),
            DaemonAction::Stop => daemon::shutdown(&base),
            DaemonAction::Status => daemon::report_status(&base),
            DaemonAction::Serve => daemon::serve(&base),
        },
        Command::Mcp => mcp_server(&base),
        Command::Check { path } => check(&base, &resolve_root(Some(&base), &path), format),
        Command::Diagnose { file } => {
            diagnose(&base, &resolve_root(Some(&base), &file), format).map(CommandOutcome::success)
        }
        Command::Context {
            symbol,
            pick,
            budget,
            only,
            match_pattern,
            around,
        } => route(
            &base,
            routing::RoutedCommand::Context {
                symbol: query_or_pick(symbol, pick.as_deref()),
                pick,
                budget,
                sections: context_sections(&only),
                match_pattern,
                around,
            },
            format,
        ),
        Command::Status => status::run(&base, format).map(CommandOutcome::success),
    }
}

/// Answers a daemon-routable command, through the root's daemon when one is live and in-process
/// otherwise. The socket is resolved here so every routed arm names one root and one lookup.
fn route(
    base: &Path,
    command: routing::RoutedCommand,
    format: Format,
) -> Result<CommandOutcome, CommandError> {
    routing::route(base, &daemon::socket_for(base)?, command, format)
}

/// Drives one bounded async engine invocation to completion on a dedicated current-thread runtime,
/// so the otherwise synchronous CLI can reuse the async passthrough adapter without a global
/// runtime.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime")
        .block_on(future)
}

/// Serves the MCP tools over stdio until the client disconnects. The tools delegate to this same
/// executable, so the server needs to know where it is; the root is canonicalized once so every
/// delegated command answers about the same workspace whatever the client's working directory.
fn mcp_server(root: &Path) -> Result<CommandOutcome, CommandError> {
    let binary = std::env::current_exe().map_err(|error| CommandError::read(root, &error))?;
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    block_on(ktsense_mcp::serve(ktsense_mcp::ServerConfig {
        binary,
        root,
    }))
    .map_err(CommandError::mcp)?;
    Ok(CommandOutcome::success(String::new()))
}

fn symbols(
    root: &Path,
    query: &str,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    pick: Option<&str>,
    contains: bool,
    format: Format,
) -> Result<CommandOutcome, CommandError> {
    if contains {
        return symbols::present_contained(root, query, kind, limit, format)
            .map(CommandOutcome::from);
    }
    let filter = symbols::PickFilter::for_query(query, pick);
    let candidates = block_on(ktsense_lsp::run_symbols(
        root,
        ktsense_core::last_segment(query),
    ))
    .map_err(|error| CommandError::passthrough(&error))?;
    // The engine's `find` does not see generated sources under `build`, so a name it answers with
    // nothing is resolved from the workspace's own syntax index, which does (KT-104). That fallback
    // also carries the generated-aware not-found wording when it finds nothing either.
    if candidates.is_empty() && matches!(filter, symbols::PickFilter::None) {
        return symbols::present_exact_from_syntax_index(root, query, kind, limit, format)
            .map(CommandOutcome::from);
    }
    symbols::present_symbols(root, query, candidates, kind, limit, filter, format)
        .map(CommandOutcome::from)
}

fn check(root: &Path, path: &Path, format: Format) -> Result<CommandOutcome, CommandError> {
    let report = block_on(ktsense_lsp::run_check(root, path))
        .map_err(|error| CommandError::passthrough(&error))?;
    let exit = if report.has_errors() {
        Exit::Failure
    } else {
        Exit::Success
    };
    Ok(CommandOutcome {
        text: render_check(&report, format)?,
        exit,
        stderr: None,
    })
}

fn diagnose(root: &Path, file: &Path, format: Format) -> Result<String, CommandError> {
    let report = block_on(ktsense_lsp::run_diagnose(root, file))
        .map_err(|error| CommandError::passthrough(&error))?;
    render_diagnose(&report, format)
}

fn render_check(report: &CheckReport, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(check_markdown(report)),
        Format::Json => as_json(report),
        Format::Dot => Err(CommandError::unsupported_format("check")),
    }
}

fn render_diagnose(report: &DiagnoseReport, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(diagnose_markdown(report)),
        Format::Json => as_json(report),
        Format::Dot => Err(CommandError::unsupported_format("diagnose")),
    }
}

/// One command answer as `--format json`: pretty-printed, with the trailing newline a shell caller
/// expects. Every `--format json` arm renders through here, so the shape and the serialization
/// failure message are stated once rather than per command.
fn as_json<T: serde::Serialize>(value: &T) -> Result<String, CommandError> {
    serde_json::to_string_pretty(value)
        .map(|json| format!("{json}\n"))
        .map_err(CommandError::serialization)
}

fn check_markdown(report: &CheckReport) -> String {
    let mut out = format!(
        "## Syntax check\n\n{} OK, {} with errors.\n",
        report.files_ok, report.files_with_errors
    );
    if report.errors.is_empty() {
        return out;
    }
    let lines: Vec<String> = report
        .errors
        .iter()
        .map(|error| {
            format!(
                "{}:{}:{}: {}",
                error.file, error.line, error.col, error.message
            )
        })
        .collect();
    out.push('\n');
    out.push_str(&fenced_block(&lines));
    out
}

fn diagnose_markdown(report: &DiagnoseReport) -> String {
    let mut out = format!("## Diagnostics: {}\n", neutralize(&report.file));
    if report.diagnostics.is_empty() {
        out.push_str("\nNo diagnostics.\n");
        return out;
    }
    let lines: Vec<String> = report
        .diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "{}:{} [{}]: {}",
                diagnostic.line,
                diagnostic.col,
                severity_word(diagnostic.severity),
                diagnostic.message
            )
        })
        .collect();
    out.push('\n');
    out.push_str(&fenced_block(&lines));
    out
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn resolve_root(root: Option<&Path>, file: &Path) -> PathBuf {
    match root {
        Some(root) if file.is_relative() => root.join(file),
        _ => file.to_path_buf(),
    }
}

fn has_kotlin_extension(file: &Path) -> bool {
    matches!(
        file.extension().and_then(|ext| ext.to_str()),
        Some("kt") | Some("kts")
    )
}

fn has_java_extension(file: &Path) -> bool {
    matches!(file.extension().and_then(|ext| ext.to_str()), Some("java"))
}

/// Outlines `file`, labelling the skeleton with its path relative to `root`, so the same file gets
/// the same heading whether the root was given as `.`, a relative path or an absolute one, and
/// whether a daemon or this process answered.
fn outline(
    root: &Path,
    file: &Path,
    format: Format,
    options: &RenderOptions,
) -> Result<String, CommandError> {
    let source = fs::read_to_string(file).map_err(|error| CommandError::read(file, &error))?;
    // tree-sitter is error tolerant and returns a tree for broken input, so `extract` recovers the
    // declarations that parsed around a localized error and marks the skeleton partial (KT-52a). It
    // fails only when the tree errors and nothing survives (whole-file garbage), which surfaces as
    // the same "syntax errors" rejection, so an unparseable file is still never a confident answer.
    let skeleton = ktsense_syntax::extract(normalized_path(root, file), &source)
        .map_err(|_| CommandError::unparseable(file))?;
    present(&skeleton, format, options)
}

fn present(
    skeleton: &FileSkeleton,
    format: Format,
    options: &RenderOptions,
) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_markdown(skeleton, options)),
        // Annotations reach JSON only under --annotations, matching the Markdown default, so a
        // client toggles them the same way in either format.
        Format::Json if options.include_annotations => as_json(skeleton),
        Format::Json => as_json(&skeleton.clone().without_annotations()),
        Format::Dot => Err(CommandError::unsupported_format("outline")),
    }
}

/// Directory names never worth walking into: version control, build output, and editor state.
///
/// `bin` is here because running `kmp-lsp` against a Gradle project triggers an Eclipse Buildship
/// import, and that import writes `bin/main/` containing verbatim copies of the `.kt` sources. A
/// traversal that descends into it counts every source twice, which silently doubles file counts and
/// puts the copy ahead of the original in centrality ranking, because `bin` sorts before `src`.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "build",
    "bin",
    ".gradle",
    ".idea",
    "node_modules",
];

/// A ceiling on directory recursion so a symlink the walk failed to skip, or a pathologically deep
/// tree, degrades to a bounded result instead of exhausting the process.
const MAX_TRAVERSAL_DEPTH: usize = 64;

fn deps(root: &Path, level: DepLevel, format: Format) -> Result<String, CommandError> {
    let files = collect_kotlin_files(root)?;
    let skeletons: Vec<FileSkeleton> = files
        .iter()
        .filter_map(|path| skeleton_for_deps(root, path))
        .collect();
    let graph = build_import_graph(&skeletons, level);
    present_deps(&graph, format)
}

/// Builds the budgeted repository map. Traversal and parsing live here so the ranking and packing
/// algorithm in core stays testable from hand-built values.
///
/// Test sources are excluded. Measured on kotlinx.coroutines, including them puts
/// `test-utils/.../MainDispatcherTestBase.kt` at the top of the map: hundreds of test files import
/// the test-utils package, so it outranks the library's own package on in-degree. A map exists to
/// answer "what is this repository", and test scaffolding is the wrong answer to that question.
/// `deps` deliberately keeps whole-repository semantics, because a dependency graph that hid half
/// the edges would be a different kind of lie.
///
/// Reference counts are gathered over the same file set that is mapped, production sources only, so
/// the ranking has one universe of discourse: a declaration's count means "referenced this often by
/// the code this map describes". Counting test references too would reintroduce the bias that
/// excluding test sources removed, by promoting whatever the test suite exercises hardest.
fn repository_map(
    root: &Path,
    budget: usize,
    compact: bool,
    focus: Option<&str>,
    fill: bool,
    path: &[String],
    format: Format,
) -> Result<String, CommandError> {
    let files = collect_kotlin_files(root)?;
    let mapped: Vec<PathBuf> = files
        .into_iter()
        .filter(|path| !is_test_source(root, path))
        .collect();
    let skeletons: Vec<FileSkeleton> = mapped
        .iter()
        .filter_map(|path| skeleton_for_deps(root, path))
        .collect();
    let references = identifiers::count_identifiers(&mapped);
    let focus_matcher = match focus {
        Some(pattern) => Some(RegexNameMatcher {
            pattern: pattern.to_string(),
            regex: Regex::new(pattern).map_err(CommandError::bad_focus_pattern)?,
        }),
        None => None,
    };
    let map = build_repo_map(
        RepoMapInput {
            files: &skeletons,
            references: &references,
            budget,
            compact,
            focus: focus_matcher.as_ref().map(|matcher| FocusSpec {
                pattern: &matcher.pattern,
                matcher,
            }),
            fill,
            path,
        },
        &ByteRatioEstimator,
    );
    present_map(&map, format)
}

/// A compiled `--focus` pattern lent to `ktsense-core` as a [`NameMatcher`], the way `--match` lends
/// a `LineMatcher`: the regex engine stays in the CLI, the pure crate matches names through the
/// trait. The pattern is kept beside the regex so the focus heading can name it.
struct RegexNameMatcher {
    pattern: String,
    regex: Regex,
}

impl NameMatcher for RegexNameMatcher {
    fn matches(&self, name: &str) -> bool {
        self.regex.is_match(name)
    }
}

/// Whether a path belongs to a test source set, by the directory conventions Gradle and the Kotlin
/// multiplatform layouts use. Matching on path segments rather than substrings keeps a production
/// file such as `contest/Manifest.kt` out of the net.
fn is_test_source(root: &Path, path: &Path) -> bool {
    const TEST_SEGMENTS: [&str; 4] = ["test", "tests", "androidTest", "test-utils"];
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| {
        let segment = component.as_os_str().to_string_lossy();
        TEST_SEGMENTS.contains(&segment.as_ref())
            || segment.ends_with("Test")
            || segment.ends_with("Tests")
    })
}

fn present_map(map: &RepoMap, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_map_markdown(map)),
        Format::Json => as_json(map),
        Format::Dot => Err(CommandError::unsupported_format("map")),
    }
}

fn present_deps(graph: &ImportGraph, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_deps_markdown(graph)),
        Format::Dot => Ok(render_deps_dot(graph)),
        Format::Json => as_json(graph),
    }
}

/// A malformed file contributes no honest edges, so it is skipped rather than aborting the whole
/// graph: one unreadable or non-UTF-8 file in a large tree must not deny an answer for the rest.
fn skeleton_for_deps(root: &Path, path: &Path) -> Option<FileSkeleton> {
    let source = fs::read_to_string(path).ok()?;
    ktsense_syntax::extract(normalized_path(root, path), &source).ok()
}

/// The path as it appears in output: relative to the workspace root and always `/`-separated, so
/// the same repository yields the same graph on every filesystem.
///
/// Paths discovered by walking the root strip directly. The engine reports absolute paths instead,
/// while `--root` is commonly relative, so those need both sides resolved against the filesystem
/// before they can be compared at all. Resolution is best effort: a path that cannot be resolved is
/// still rendered, because a readable absolute path beats an error for what is only a display
/// concern.
fn normalized_path(root: &Path, path: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(root) {
        return join_components(relative);
    }
    let resolved_root = fs::canonicalize(root);
    let resolved_path = fs::canonicalize(path);
    let root_anchor = resolved_root.as_deref().unwrap_or(root);
    let path_anchor = resolved_path.as_deref().unwrap_or(path);
    join_components(path_anchor.strip_prefix(root_anchor).unwrap_or(path_anchor))
}

/// Joins components with `/`, preserving a single leading slash when the path is still absolute.
///
/// `Component::RootDir` already renders as `/`, so joining it with a separator like any other
/// component yields a doubled leading slash.
fn join_components(path: &Path) -> String {
    let joined = path
        .components()
        .filter(|component| !matches!(component, Component::RootDir))
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if path.has_root() {
        format!("/{joined}")
    } else {
        joined
    }
}

fn collect_kotlin_files(root: &Path) -> Result<Vec<PathBuf>, CommandError> {
    collect_source_files(root, has_kotlin_extension)
}

/// Every `.java` file under `root`, walked and pruned exactly as [`collect_kotlin_files`] walks the
/// Kotlin sources, so a mixed workspace's Java text scan sees the same tree the Kotlin answer does
/// (KT-112). An all-Kotlin workspace yields an empty list, which leaves `trace` and `context`
/// byte-identical to before.
pub(crate) fn collect_java_files(root: &Path) -> Result<Vec<PathBuf>, CommandError> {
    collect_source_files(root, has_java_extension)
}

/// Walks `root`, returning every file the `keep` predicate accepts, skipping symlinks and the
/// ignored directory names, bounded by [`MAX_TRAVERSAL_DEPTH`]. Shared by the Kotlin and Java
/// collectors so the two walk one tree the same way.
fn collect_source_files(
    root: &Path,
    keep: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, CommandError> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let mut children = read_child_paths(&directory)?;
        children.sort();
        for child in children {
            let metadata =
                fs::symlink_metadata(&child).map_err(|error| CommandError::read(&child, &error))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth < MAX_TRAVERSAL_DEPTH && !is_ignored_dir(&child) {
                    pending.push((child, depth + 1));
                }
            } else if keep(&child) {
                files.push(child);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn read_child_paths(directory: &Path) -> Result<Vec<PathBuf>, CommandError> {
    let entries = fs::read_dir(directory).map_err(|error| CommandError::read(directory, &error))?;
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| CommandError::read(directory, &error))
        })
        .collect()
}

/// The build-output subdirectories that hold Kotlin a declaration lookup should read even though
/// `build` itself is ignored for every other traversal. KSP writes `build/generated/ksp/...` and
/// kapt `build/generated/source/kapt/...`, both under `build/generated`; some plugins use
/// `build/generated-src`. All four acceptance paths are reached by descending into these two
/// children of any `build` directory.
const GENERATED_DIRS: &[&str] = &["generated", "generated-src"];

/// Every generated Kotlin file under `root`: a `.kt` beneath a module's `build/generated` or
/// `build/generated-src`. This is the narrow opt-in walk the card calls for: `build` stays in
/// [`IGNORED_DIRS`], so `map`, `outline` and `deps` never see a generated copy, and only a
/// declaration lookup reaches here. It descends into `build` solely to reach the generated subtrees,
/// never the compiled output beside them. Best effort: an unreadable directory contributes nothing
/// rather than failing the lookup, because generated sources are a bonus a broken build must not
/// deny the rest of the answer.
pub(crate) fn collect_generated_kotlin_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(mut children) = read_child_paths(&directory) else {
            continue;
        };
        children.sort();
        for child in children {
            if !is_real_directory(&child) {
                continue;
            }
            if is_build_dir(&child) {
                collect_generated_under_build(&child, &mut files);
            } else if depth < MAX_TRAVERSAL_DEPTH && !is_ignored_dir(&child) {
                pending.push((child, depth + 1));
            }
        }
    }
    files.sort();
    files
}

/// Collects every `.kt` under the generated subtrees of one `build` directory, leaving the compiled
/// output beside them untouched.
fn collect_generated_under_build(build_dir: &Path, files: &mut Vec<PathBuf>) {
    for generated in GENERATED_DIRS {
        collect_kotlin_under(&build_dir.join(generated), 0, files);
    }
}

/// Appends every `.kt` beneath `directory`, recursively and symlink-free, bounded by the traversal
/// depth. Unlike [`collect_kotlin_files`] this does not prune ignored directory names: a generated
/// tree carries no `.git` or `target`, and a nested `build` under `build/generated` is still
/// generated output worth reading.
fn collect_kotlin_under(directory: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > MAX_TRAVERSAL_DEPTH {
        return;
    }
    let Ok(children) = read_child_paths(directory) else {
        return;
    };
    for child in children {
        match fs::symlink_metadata(&child) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                collect_kotlin_under(&child, depth + 1, files)
            }
            Ok(metadata) if metadata.file_type().is_file() && has_kotlin_extension(&child) => {
                files.push(child)
            }
            _ => {}
        }
    }
}

/// Whether `path` is a directory and not a symlink, so a walk neither follows a link out of the tree
/// nor mistakes one for a real directory.
fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Whether a directory is a Gradle `build` output directory, the one place a declaration lookup
/// descends past [`IGNORED_DIRS`] to reach generated sources.
fn is_build_dir(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("build")
}

fn is_ignored_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') || IGNORED_DIRS.contains(&name))
}

/// Wraps engine-derived lines in a `text` code fence sized to survive any backtick run they carry,
/// with each line neutralized first. A diagnostic message quoting source cannot then break out of
/// the block or reorder what the reader sees.
///
/// The boundary itself is `ktsense_core::text`, shared with the skeleton renderer: passthrough text
/// is source derived and reaches an LLM reader exactly as a rendered signature does, so both cross
/// the same one. Until KT-69 this crate carried a byte-identical copy because core's version was
/// private to that crate.
fn fenced_block(lines: &[String]) -> String {
    let body: Vec<String> = lines
        .iter()
        .map(|line| neutralize(line).into_owned())
        .collect();
    let joined = body.join("\n");
    let fence = fence_for(&joined);
    format!("{fence}text\n{joined}\n{fence}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktsense_lsp::{CheckReport, DiagnoseReport, Diagnostic, Severity, SyntaxError};

    #[test]
    fn engine_absolute_paths_render_root_relative_and_never_double_the_leading_slash() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        let nested = root.join("core/src/main/kotlin/A.kt");
        fs::create_dir_all(nested.parent().expect("parent")).expect("create tree");
        fs::write(&nested, "package a\n").expect("write file");
        // A root carrying a `..` cannot be stripped textually, so this is what forces the
        // filesystem-resolving fallback that an engine-supplied absolute path depends on.
        let unresolved_root = root.join("..").join("root");

        let observed = (
            normalized_path(&unresolved_root, &nested),
            normalized_path(&root, &nested),
            normalized_path(&root, Path::new("/elsewhere/Outside.kt")),
            normalized_path(&root, Path::new("already/relative.kt")),
        );

        assert_eq!(
            observed,
            (
                "core/src/main/kotlin/A.kt".to_string(),
                "core/src/main/kotlin/A.kt".to_string(),
                "/elsewhere/Outside.kt".to_string(),
                "already/relative.kt".to_string(),
            )
        );
    }

    #[test]
    fn a_buildship_bin_copy_of_a_source_is_not_walked_alongside_the_original() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        let source = "package shop\n";
        for relative in [
            "core/src/main/kotlin/shop/A.kt",
            "core/bin/main/shop/A.kt",
            "core/build/classes/kotlin/shop/A.kt",
            ".gradle/cached/shop/A.kt",
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create tree");
            fs::write(&path, source).expect("write file");
        }

        let walked: Vec<String> = collect_kotlin_files(&root)
            .expect("walks the tree")
            .iter()
            .map(|path| normalized_path(&root, path))
            .collect();

        assert_eq!(walked, ["core/src/main/kotlin/shop/A.kt"]);
    }

    #[test]
    fn the_generated_walk_reads_only_generated_subtrees_of_build_and_nothing_else() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        let source = "package shop\n";
        for relative in [
            "core/src/main/kotlin/shop/Regular.kt",
            "core/build/generated/ksp/main/kotlin/shop/GenKsp.kt",
            "core/build/generated/source/kapt/main/shop/GenKapt.kt",
            "app/build/generated-src/shop/GenSrc.kt",
            "core/build/classes/kotlin/shop/Compiled.kt",
            "core/build/tmp/shop/Scratch.kt",
            ".git/hooks/shop/Hook.kt",
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create tree");
            fs::write(&path, source).expect("write file");
        }

        let mut generated: Vec<String> = collect_generated_kotlin_files(&root)
            .iter()
            .map(|path| normalized_path(&root, path))
            .collect();
        generated.sort();

        assert_eq!(
            generated,
            [
                "app/build/generated-src/shop/GenSrc.kt",
                "core/build/generated/ksp/main/kotlin/shop/GenKsp.kt",
                "core/build/generated/source/kapt/main/shop/GenKapt.kt",
            ]
        );
    }

    #[test]
    fn exit_codes_are_the_documented_contract() {
        let observed = (
            Exit::Success.code(),
            Exit::Failure.code(),
            Exit::Usage.code(),
            Exit::Ambiguous.code(),
            Exit::Unimplemented.code(),
        );

        assert_eq!(observed, (0, 1, 2, 3, 70));
    }

    fn check_error(message: &str) -> CheckReport {
        CheckReport {
            files_ok: 0,
            files_with_errors: 1,
            errors: vec![SyntaxError {
                file: "src/X.kt".to_string(),
                line: 1,
                col: 1,
                message: message.to_string(),
            }],
        }
    }

    #[test]
    fn check_markdown_states_the_census_and_fences_reported_errors() {
        let clean = CheckReport {
            files_ok: 3,
            files_with_errors: 0,
            errors: Vec::new(),
        };

        let observed = (
            check_markdown(&clean),
            check_markdown(&check_error("unexpected `fun`")),
        );
        assert_eq!(
            observed,
            (
                "## Syntax check\n\n3 OK, 0 with errors.\n".to_string(),
                concat!(
                    "## Syntax check\n\n0 OK, 1 with errors.\n\n",
                    "```text\n",
                    "src/X.kt:1:1: unexpected `fun`\n",
                    "```\n",
                )
                .to_string(),
            )
        );
    }

    #[test]
    fn a_message_with_a_fence_and_a_newline_is_widened_and_neutralized_not_left_to_break_out() {
        assert_eq!(
            check_markdown(&check_error("a\n``` b")),
            concat!(
                "## Syntax check\n\n0 OK, 1 with errors.\n\n",
                "````text\n",
                "src/X.kt:1:1: a<U+000A>``` b\n",
                "````\n",
            )
        );
    }

    #[test]
    fn diagnose_markdown_says_none_or_fences_findings_with_a_neutralized_heading() {
        let clean = DiagnoseReport {
            file: "ok.kt\n## Injected".to_string(),
            diagnostics: Vec::new(),
        };
        let findings = DiagnoseReport {
            file: "src/When.kt".to_string(),
            diagnostics: vec![Diagnostic {
                line: 3,
                col: 15,
                severity: Severity::Warning,
                message: "'when' is missing branches: B".to_string(),
            }],
        };

        let observed = (diagnose_markdown(&clean), diagnose_markdown(&findings));
        assert_eq!(
            observed,
            (
                "## Diagnostics: ok.kt<U+000A>## Injected\n\nNo diagnostics.\n".to_string(),
                concat!(
                    "## Diagnostics: src/When.kt\n\n",
                    "```text\n",
                    "3:15 [warning]: 'when' is missing branches: B\n",
                    "```\n",
                )
                .to_string(),
            )
        );
    }

    #[test]
    fn json_rendering_reflects_the_normalized_report() {
        let report = check_error("boom");
        assert_eq!(
            render_check(&report, Format::Json).unwrap(),
            serde_json::to_string_pretty(&report).unwrap() + "\n"
        );
    }

    /// KT-119: `--pick` stands alone as the query on `symbols`, `trace` and `context`. The positional
    /// is optional when `--pick` is present, parses to `None`, and the engine lookup name is derived
    /// from the pick's last dot segment; an explicit positional wins; and an invocation with neither
    /// the positional nor `--pick` is a clap usage error (exit 2).
    #[test]
    fn pick_stands_alone_as_the_query_and_neither_is_a_usage_error() {
        let symbols =
            Cli::try_parse_from(["ktsense", "symbols", "--pick", "a.b.UpdateDocumentBase"]);
        let symbols_fields = match symbols.map(|cli| cli.command) {
            Ok(Command::Symbols { query, pick, .. }) => (query, pick),
            _ => (Some("parse failed".to_string()), None),
        };
        let trace_ok =
            Cli::try_parse_from(["ktsense", "trace", "--pick", "shop.Repo.save"]).is_ok();
        let context_ok = Cli::try_parse_from(["ktsense", "context", "--pick", "x.y.Z.run"]).is_ok();
        let neither_all_usage_errors = [
            Cli::try_parse_from(["ktsense", "symbols"]),
            Cli::try_parse_from(["ktsense", "trace"]),
            Cli::try_parse_from(["ktsense", "context"]),
        ]
        .iter()
        .all(|parsed| {
            parsed
                .as_ref()
                .err()
                .is_some_and(|error| error.use_stderr())
        });

        let observed = (
            symbols_fields,
            trace_ok,
            context_ok,
            query_or_pick(None, Some("a.b.UpdateDocumentBase")),
            query_or_pick(None, Some("InventoryItemsRepository.createProduct")),
            query_or_pick(Some("explicit".to_string()), Some("a.b.C")),
            neither_all_usage_errors,
        );
        assert_eq!(
            observed,
            (
                (None, Some("a.b.UpdateDocumentBase".to_string())),
                true,
                true,
                "UpdateDocumentBase".to_string(),
                "createProduct".to_string(),
                "explicit".to_string(),
                true,
            )
        );
    }
}
