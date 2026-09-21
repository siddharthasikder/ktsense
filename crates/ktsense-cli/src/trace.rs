//! The `trace` command: one symbol's definition, implementors, callers and every reference site.
//!
//! Resolution reuses the `symbols` path, so an ambiguous name lists its candidates and exits 3
//! exactly as `symbols` does. The engine work is one LSP session per invocation: initialize, wait
//! for the index to be Ready or for the cap to elapse, ask for implementations and references at
//! the declaration, repeat references at each caller for every further level of `--depth`, then
//! shut the session down. The phase the wait reached travels into the answer as its `index`
//! marker, so a list read off a still-building index is presented as the lower bound it is.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ktsense_core::{
    build_trace, callers_of, render_trace_markdown, Definition, FileSkeleton, GroupingOptions,
    IndexCompleteness, Location, RelatedDeclaration, TraceInput, TraceReport,
};
use ktsense_lsp::{
    wait_for_index, DeclarationScope, FilePosition, IndexPhase, InitializeConfig, LspClient,
    LspError, SiteLocation, SymbolCandidate,
};

use crate::symbols::{self, Selection};
use crate::{block_on, normalized_path, CommandError, CommandOutcome, Format};

/// How long a capped cold session may spend waiting for the index before answering with what it
/// has; `KTSENSE_INDEX_CAP_MS` overrides it, which the tests use to shorten it.
const DEFAULT_INDEX_CAP: Duration = Duration::from_secs(3);
const INDEX_CAP_ENV: &str = "KTSENSE_INDEX_CAP_MS";
const IGNORED_BUILD_OUTPUT: &str = "**/build/**";

pub(crate) struct TraceRequest<'a> {
    pub root: &'a Path,
    pub symbol: &'a str,
    pub pick: Option<&'a str>,
    pub depth: usize,
    pub limit: Option<usize>,
    pub wait: IndexWaitPolicy,
    pub format: Format,
}

/// How long a cold session waits for the index before answering. The cap is the default because
/// KT-53 measured genuinely cold indexing at 1.04 s on the largest pinned corpus, a third of it;
/// `--wait-index` trades that bound for a guaranteed `index: complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexWaitPolicy {
    Capped,
    UntilReady,
}

impl IndexWaitPolicy {
    pub(crate) fn from_flag(wait_index: bool) -> Self {
        if wait_index {
            IndexWaitPolicy::UntilReady
        } else {
            IndexWaitPolicy::Capped
        }
    }

    fn cap(self) -> Duration {
        match self {
            IndexWaitPolicy::Capped => configured_cap(),
            IndexWaitPolicy::UntilReady => UNBOUNDED_WAIT,
        }
    }
}

/// Long enough that no index finishes after it; the waiter still returns when the stream closes.
const UNBOUNDED_WAIT: Duration = Duration::from_secs(60 * 60 * 24);

/// What resolving a symbol and running one engine session produced: the answer, or the candidate
/// list to print when the name was ambiguous. Shared with `context`, so both commands inherit the
/// same ambiguity contract and drive the same session rather than each talking to the engine their
/// own way.
pub(crate) enum Traced {
    Resolved(TraceReport),
    Ambiguous(CommandOutcome),
}

pub(crate) fn trace(request: TraceRequest<'_>) -> Result<CommandOutcome, CommandError> {
    match resolve(&request)? {
        Traced::Resolved(report) => present(&report, request.format).map(CommandOutcome::success),
        Traced::Ambiguous(outcome) => Ok(outcome),
    }
}

pub(crate) fn resolve(request: &TraceRequest<'_>) -> Result<Traced, CommandError> {
    let candidates = block_on(ktsense_lsp::run_symbols(request.root, request.symbol))
        .map_err(|error| CommandError::passthrough(&error))?;
    let (candidate, resolved) = match symbols::select(
        request.root,
        request.symbol,
        candidates,
        request.pick,
        request.format,
    )? {
        Selection::One(candidate, resolved) => (candidate, resolved),
        Selection::Ambiguous(outcome) => return Ok(Traced::Ambiguous(outcome.into())),
    };

    let definition = Definition {
        qualified_name: resolved.fqn,
        path: resolved.file,
        line: resolved.line,
        signature: resolved.signature,
    };
    let report =
        block_on(collect(request, &candidate, definition)).map_err(CommandError::engine)?;
    Ok(Traced::Resolved(report))
}

/// Runs the whole engine session and always tears it down, whatever the requests returned.
async fn collect(
    request: &TraceRequest<'_>,
    candidate: &SymbolCandidate,
    definition: Definition,
) -> Result<TraceReport, LspError> {
    let mut client = ktsense_lsp::launch().await?;
    let outcome = Session::new(request.root, request.limit, request.wait)
        .run(&mut client, candidate, definition, request.depth)
        .await;
    let _ = client.shutdown().await;
    outcome
}

/// The state one trace accumulates while talking to the engine: the workspace root every path is
/// shown relative to, the per-file cap, and the skeletons of every file the answer has touched.
struct Session<'a> {
    root: &'a Path,
    /// The root as the engine must see it: every URI the session sends is built from this, since
    /// a relative `--root` would otherwise produce a relative `file://` URI the engine cannot open.
    canonical_root: PathBuf,
    limit: Option<usize>,
    wait: IndexWaitPolicy,
    skeletons: Skeletons,
}

impl<'a> Session<'a> {
    fn new(root: &'a Path, limit: Option<usize>, wait: IndexWaitPolicy) -> Self {
        Self {
            root,
            canonical_root: canonical(root),
            limit,
            wait,
            skeletons: Skeletons::default(),
        }
    }

    async fn run(
        mut self,
        client: &mut LspClient,
        candidate: &SymbolCandidate,
        definition: Definition,
        depth: usize,
    ) -> Result<TraceReport, LspError> {
        let config = InitializeConfig {
            root_uri: format!("file://{}", self.canonical_root.display()),
            ignore_patterns: vec![IGNORED_BUILD_OUTPUT.to_string()],
        };
        client.initialize(&config).await?;
        let wait = wait_for_index(client, self.wait.cap()).await;

        let at = self.declaration_position(candidate);
        let implementation_sites = self.sites(&client.implementation_sites(&at).await?);
        let reference_sites = self.sites(
            &client
                .reference_sites(&at, DeclarationScope::Included)
                .await?,
        );

        let definition_site = Location::new(definition.path.clone(), definition.line);
        self.skeletons.load(self.root, &definition.path);
        let options = self.grouping();
        let mut report = build_trace(TraceInput {
            definition,
            index: completeness(wait.phase),
            definition_site,
            implementation_sites,
            reference_sites,
            skeletons: self.skeletons.all(),
            options,
        });

        let mut frontier: Vec<RelatedDeclaration> = report.direct_callers().to_vec();
        for _ in 1..depth {
            if frontier.is_empty() {
                break;
            }
            let next = self.callers_of_each(client, &frontier).await?;
            frontier = next.clone();
            report = report.with_deeper_callers(next);
        }
        Ok(report)
    }

    /// The declarations referring to any of `callers`, asked one caller at a time at the position
    /// of its name; a caller whose declaration could not be located contributes nothing.
    async fn callers_of_each(
        &mut self,
        client: &LspClient,
        callers: &[RelatedDeclaration],
    ) -> Result<Vec<RelatedDeclaration>, LspError> {
        let mut found = Vec::new();
        for caller in callers {
            let Some(at) = self.position_of(caller) else {
                continue;
            };
            let references = client
                .reference_sites(&at, DeclarationScope::Excluded)
                .await?;
            let sites = self.sites(&references);
            let own = Location::new(caller.path.clone(), caller.line);
            found.extend(callers_of(&sites, &[own], self.skeletons.all()));
        }
        found.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        found.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(found)
    }

    /// The LSP position of a caller's own name, found on its declaration line, so its references
    /// can be asked for in turn.
    fn position_of(&self, caller: &RelatedDeclaration) -> Option<FilePosition> {
        let name = caller.qualified_name.as_deref()?.rsplit('.').next()?;
        let absolute = self.absolute(&caller.path);
        name_position(&absolute, caller.line, name)
    }

    /// Where to ask the engine about the resolved declaration. The engine's own column is not
    /// trusted: on a cold cache its command-mode `find` takes a text-search path and has been
    /// observed to report the column of the keyword before the name (`val CallLogging` at the
    /// `v`), and a references request there answers with every use of the keyword, 14,702 sites on
    /// ktor instead of 31. The name is located on the reported line instead, and the engine's
    /// column is used only when the name cannot be found there.
    fn declaration_position(&self, candidate: &SymbolCandidate) -> FilePosition {
        name_position(Path::new(&candidate.file), candidate.line, &candidate.name)
            .unwrap_or_else(|| candidate.to_file_position())
    }

    fn sites(&mut self, locations: &[SiteLocation]) -> Vec<Location> {
        locations
            .iter()
            .map(|site| {
                let path = normalized_path(self.root, &site.path);
                self.skeletons.load(self.root, &path);
                Location::new(path, site.line).at_column(site.column)
            })
            .collect()
    }

    fn grouping(&self) -> GroupingOptions {
        match self.limit {
            Some(limit) => GroupingOptions::default().with_limit(limit),
            None => GroupingOptions::default(),
        }
    }

    fn absolute(&self, path: &str) -> PathBuf {
        if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.canonical_root.join(path)
        }
    }
}

/// The zero-based LSP position of the first whole-word occurrence of `name` on 1-based `line` of
/// the file at `path`, or `None` when the file cannot be read or the name is not on that line. The
/// whole-word search is centralized in `ktsense-lsp` so the resolver and this command agree.
fn name_position(path: &Path, line: u32, name: &str) -> Option<FilePosition> {
    let column = ktsense_lsp::name_column(path, line, name)?;
    Some(FilePosition {
        uri: format!("file://{}", path.display()),
        line: line - 1,
        character: column - 1,
    })
}

fn completeness(phase: IndexPhase) -> IndexCompleteness {
    if phase.is_ready() {
        IndexCompleteness::Complete
    } else {
        IndexCompleteness::Partial
    }
}

fn configured_cap() -> Duration {
    std::env::var(INDEX_CAP_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_INDEX_CAP)
}

fn canonical(root: &Path) -> PathBuf {
    fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The skeletons of every file the answer touches, parsed once each. A file that cannot be read or
/// parsed is simply absent, and its sites then render without an enclosing declaration rather than
/// failing the whole answer.
#[derive(Default)]
struct Skeletons {
    by_path: BTreeMap<String, FileSkeleton>,
    ordered: Vec<FileSkeleton>,
    stale: bool,
}

impl Skeletons {
    fn load(&mut self, root: &Path, path: &str) {
        if self.by_path.contains_key(path) {
            return;
        }
        let Some(skeleton) = skeleton_at(root, path) else {
            return;
        };
        self.by_path.insert(path.to_string(), skeleton);
        self.stale = true;
    }

    fn all(&mut self) -> &[FileSkeleton] {
        if self.stale {
            self.ordered = self.by_path.values().cloned().collect();
            self.stale = false;
        }
        &self.ordered
    }
}

/// The skeleton of one file named as the answer names it, or `None` when it cannot be read or
/// parsed. Shared with `context`, which outlines the file a declaration lives in.
pub(crate) fn skeleton_at(root: &Path, path: &str) -> Option<FileSkeleton> {
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let source = fs::read_to_string(&absolute).ok()?;
    ktsense_syntax::extract(path.to_string(), &source).ok()
}

fn present(report: &TraceReport, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_trace_markdown(report)),
        Format::Json => serde_json::to_string_pretty(report).map_err(CommandError::serialization),
        Format::Dot => Err(CommandError::unsupported_format("trace")),
    }
}
