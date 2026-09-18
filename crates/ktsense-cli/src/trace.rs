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

/// How long a cold session may spend waiting for the index before answering with what it has.
/// KT-24 owns turning this into a policy knob; `KTSENSE_INDEX_CAP_MS` exists so tests can shorten it.
const DEFAULT_INDEX_CAP: Duration = Duration::from_secs(3);
const INDEX_CAP_ENV: &str = "KTSENSE_INDEX_CAP_MS";
const IGNORED_BUILD_OUTPUT: &str = "**/build/**";

pub(crate) struct TraceRequest<'a> {
    pub root: &'a Path,
    pub symbol: &'a str,
    pub pick: Option<&'a str>,
    pub depth: usize,
    pub limit: Option<usize>,
    pub format: Format,
}

pub(crate) fn trace(request: TraceRequest<'_>) -> Result<CommandOutcome, CommandError> {
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
        Selection::Ambiguous(outcome) => return Ok(outcome.into()),
    };

    let definition = Definition {
        qualified_name: resolved.fqn,
        path: resolved.file,
        line: resolved.line,
        signature: resolved.signature,
    };
    let report =
        block_on(collect(&request, &candidate, definition)).map_err(CommandError::engine)?;
    present(&report, request.format).map(CommandOutcome::success)
}

/// Runs the whole engine session and always tears it down, whatever the requests returned.
async fn collect(
    request: &TraceRequest<'_>,
    candidate: &SymbolCandidate,
    definition: Definition,
) -> Result<TraceReport, LspError> {
    let mut client = ktsense_lsp::launch().await?;
    let outcome = Session::new(request.root, request.limit)
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
    skeletons: Skeletons,
}

impl<'a> Session<'a> {
    fn new(root: &'a Path, limit: Option<usize>) -> Self {
        Self {
            root,
            canonical_root: canonical(root),
            limit,
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
        let wait = wait_for_index(client, index_cap()).await;

        let at = candidate.to_file_position();
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
        let source = fs::read_to_string(&absolute).ok()?;
        let text = source.lines().nth(caller.line.checked_sub(1)? as usize)?;
        let column = text.find(name)?;
        Some(FilePosition {
            uri: format!("file://{}", absolute.display()),
            line: caller.line - 1,
            character: u32::try_from(text[..column].chars().count()).ok()?,
        })
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

fn completeness(phase: IndexPhase) -> IndexCompleteness {
    if phase.is_ready() {
        IndexCompleteness::Complete
    } else {
        IndexCompleteness::Partial
    }
}

fn index_cap() -> Duration {
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
        let absolute = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        let Ok(source) = fs::read_to_string(&absolute) else {
            return;
        };
        let Ok(skeleton) = ktsense_syntax::extract(path.to_string(), &source) else {
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

fn present(report: &TraceReport, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_trace_markdown(report)),
        Format::Json => serde_json::to_string_pretty(report).map_err(CommandError::serialization),
        Format::Dot => Err(CommandError::unsupported_format("trace")),
    }
}
