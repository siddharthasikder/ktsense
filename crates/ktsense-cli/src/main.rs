//! `ktsense` entry point.
//!
//! Commands are declared here in full from the start so the CLI surface, the MCP tool catalogue and
//! the documentation cannot drift apart. Each one that has not landed yet reports that it is not
//! implemented, which keeps `--help` honest instead of advertising behaviour that does not exist.

#![forbid(unsafe_code)]

mod symbols;

use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use ktsense_core::{
    build_import_graph, render_deps_dot, render_deps_markdown, render_markdown, DepLevel,
    FileSkeleton, ImportGraph, RenderOptions,
};
use ktsense_lsp::{CheckReport, DiagnoseReport, PassthroughError, Severity};

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
    Symbols {
        query: String,
        /// Keep only declarations of this kind.
        #[arg(long, value_enum)]
        kind: Option<KindFilter>,
        /// Show at most this many rows when the name is ambiguous; the rest are summarized.
        #[arg(long)]
        limit: Option<usize>,
        /// Select the single candidate with this fully-qualified name and exit successfully.
        #[arg(long, value_name = "FQN")]
        pick: Option<String>,
    },
    /// Definition, usages, implementors and callers of one symbol
    Trace { symbol: String },
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
    },
    /// Syntax-check files; exits non-zero when a file has errors
    Check { path: PathBuf },
    /// Semantic diagnostics on one file (requires the engine index)
    Diagnose { file: PathBuf },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Success,
    Failure,
    Usage,
    Ambiguous,
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

    fn unsupported_format(command: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {command} does not support --format dot (only deps does)"),
        }
    }

    fn passthrough(error: &PassthroughError) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: {error}"),
        }
    }

    fn no_symbol(query: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: no declaration named {query}"),
        }
    }

    fn pick_missed(pick: &str) -> Self {
        Self {
            exit: Exit::Failure,
            message: format!("ktsense: no candidate has the fully-qualified name {pick}"),
        }
    }
}

/// A completed command: the text to print on stdout and the status the process should end with.
/// Most commands succeed, but `check` must be able to print its report and still exit non-zero, so
/// the exit travels with the output rather than being inferred from success or failure alone.
struct CommandOutcome {
    text: String,
    exit: Exit,
}

impl CommandOutcome {
    fn success(text: String) -> Self {
        Self {
            text,
            exit: Exit::Success,
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
    let root = cli.root;
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
            outline(&resolve_root(root.as_deref(), &file), format, &options)
                .map(CommandOutcome::success)
        }
        Command::Deps { level } => {
            let base = root.unwrap_or_else(|| PathBuf::from("."));
            deps(&base, level.into(), format).map(CommandOutcome::success)
        }
        Command::Symbols {
            query,
            kind,
            limit,
            pick,
        } => {
            let base = root.unwrap_or_else(|| PathBuf::from("."));
            symbols(&base, &query, kind, limit, pick.as_deref(), format)
        }
        Command::Check { path } => {
            let base = root.unwrap_or_else(|| PathBuf::from("."));
            check(&base, &resolve_root(Some(&base), &path), format)
        }
        Command::Diagnose { file } => {
            let base = root.unwrap_or_else(|| PathBuf::from("."));
            diagnose(&base, &resolve_root(Some(&base), &file), format).map(CommandOutcome::success)
        }
        ref pending => Err(CommandError::unimplemented(not_implemented_label(pending))),
    }
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

fn symbols(
    root: &Path,
    query: &str,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    pick: Option<&str>,
    format: Format,
) -> Result<CommandOutcome, CommandError> {
    let candidates = block_on(ktsense_lsp::run_symbols(root, query))
        .map_err(|error| CommandError::passthrough(&error))?;
    symbols::present_symbols(root, query, candidates, kind, limit, pick, format)
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
        Format::Json => serde_json::to_string_pretty(report)
            .map(|json| format!("{json}\n"))
            .map_err(CommandError::serialization),
        Format::Dot => Err(CommandError::unsupported_format("check")),
    }
}

fn render_diagnose(report: &DiagnoseReport, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(diagnose_markdown(report)),
        Format::Json => serde_json::to_string_pretty(report)
            .map(|json| format!("{json}\n"))
            .map_err(CommandError::serialization),
        Format::Dot => Err(CommandError::unsupported_format("diagnose")),
    }
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
        Format::Dot => Err(CommandError::unsupported_format("outline")),
    }
}

/// Directory names never worth walking into: version control, build output, and editor state.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "build",
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

fn present_deps(graph: &ImportGraph, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_deps_markdown(graph)),
        Format::Dot => Ok(render_deps_dot(graph)),
        Format::Json => serde_json::to_string_pretty(graph)
            .map(|json| format!("{json}\n"))
            .map_err(CommandError::serialization),
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
            } else if has_kotlin_extension(&child) {
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

fn is_ignored_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') || IGNORED_DIRS.contains(&name))
}

/// Bidirectional-override codepoints (the "Trojan Source" set, CVE-2021-42574). They are Unicode
/// category Cf, so `char::is_control` misses them, yet they reorder how text renders. The core
/// renderer neutralizes the same set on skeleton output; passthrough text is likewise source
/// derived and reaches an LLM reader, so it passes through an equivalent boundary rather than being
/// emitted raw. Core's boundary is private to that crate, hence this local equivalent.
const BIDIRECTIONAL_OVERRIDES: [char; 12] = [
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}',
];

const MIN_FENCE_BACKTICKS: usize = 3;

/// Wraps engine-derived lines in a `text` code fence sized to survive any backtick run they carry,
/// with each line neutralized first. A diagnostic message quoting source cannot then break out of
/// the block or reorder what the reader sees.
fn fenced_block(lines: &[String]) -> String {
    let body: Vec<String> = lines
        .iter()
        .map(|line| neutralize(line).into_owned())
        .collect();
    let joined = body.join("\n");
    let fence = fence_for(&joined);
    format!("{fence}text\n{joined}\n{fence}\n")
}

/// A fence one backtick longer than the longest backtick run in `body`, never shorter than
/// [`MIN_FENCE_BACKTICKS`], so a backtick run inside the body sits as text instead of closing it.
fn fence_for(body: &str) -> String {
    let longest_run = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat((longest_run + 1).max(MIN_FENCE_BACKTICKS))
}

/// Source-derived text made safe to embed in a line of Markdown: a line break, any other control
/// character, or a bidirectional override becomes a visible `<U+XXXX>` marker; every other byte is
/// left untouched. Backtick runs are the fence's job, not this one's.
fn neutralize(text: &str) -> Cow<'_, str> {
    if !text.contains(is_unsafe_in_output) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if is_unsafe_in_output(character) {
            escaped.push_str(&format!("<U+{:04X}>", character as u32));
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

fn is_unsafe_in_output(character: char) -> bool {
    character.is_control() || BIDIRECTIONAL_OVERRIDES.contains(&character)
}

fn not_implemented_label(command: &Command) -> &'static str {
    match command {
        Command::Outline { .. } => "outline (KT-07)",
        Command::Symbols { .. } => "symbols (KT-16)",
        Command::Trace { .. } => "trace (KT-18)",
        Command::Deps { .. } => "deps (KT-20)",
        Command::Map { .. } => "map (KT-22)",
        Command::Check { .. } => "check (KT-23)",
        Command::Diagnose { .. } => "diagnose (KT-23)",
        Command::Context { .. } => "context (KT-35)",
        Command::Status => "status (KT-36)",
        Command::Mcp => "mcp (KT-31)",
        Command::Daemon { .. } => "daemon (KT-27)",
    }
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
}
