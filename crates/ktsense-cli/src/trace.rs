//! The `trace` command: one symbol's definition, implementors, callers and every reference site.
//!
//! Resolution reuses the `symbols` path, so an ambiguous name lists its candidates and exits 3
//! exactly as `symbols` does. The engine work is one LSP session per invocation: initialize, wait
//! for the index to be Ready or for the cap to elapse, ask for implementations and references at
//! the declaration, repeat references at each caller for every further level of `--depth`, then
//! shut the session down. The phase the wait reached travels into the answer as its `index`
//! marker, so a list read off a still-building index is presented as the lower bound it is.
//!
//! A routed trace shares all of that but neither of the two subprocesses. The session is the
//! daemon's own warm one, and the candidates come from that session's index through
//! [`ktsense_daemon::resolve_from_warm_index`] rather than from a command-mode `find` child, which
//! rebuilds the engine's whole index per invocation and cost more than the rest of a warm trace put
//! together. Both paths then hand their candidates to the same [`select_candidate`], so the
//! ambiguity contract, `--pick` and the no-such-symbol error are the same code whichever answered.
//!
//! The two paths reach for the same two resolvers in opposite orders, because they start from
//! opposite assets. A routed trace already holds a settled index and no spare subprocess, so it asks
//! the index first and falls back to `find`. A fresh trace holds a cheap subprocess and no session
//! yet, so it asks `find` first, which is what lets an ambiguous name exit without a session at all,
//! and only when `find` reports nothing does it open the session it was about to open anyway and ask
//! that index before agreeing the name is absent. Neither path reports absence on the strength of a
//! cold `find` alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ktsense_core::{
    build_trace, callers_of, render_trace_markdown, Definition, FileSkeleton, GroupingOptions,
    IndexCompleteness, Location, RelatedDeclaration, TraceInput, TraceReport,
};
use ktsense_daemon::{WarmEngine, WarmResolution};
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
/// The build-output glob a trace session tells the engine to ignore, so implementors and references
/// never point at a Buildship `bin/` copy of a source. The daemon's warm session is initialized with
/// the same pattern, so a routed trace answers about the same files the fresh path does.
pub(crate) const IGNORED_BUILD_OUTPUT: &str = "**/build/**";

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

/// The in-process `trace`: resolve the name, then answer from a fresh engine session. This is the
/// fallback path a routed `trace` degrades to when no daemon is live, and the path `context` reuses
/// through [`resolve`].
pub(crate) fn trace(request: TraceRequest<'_>) -> Result<CommandOutcome, CommandError> {
    match resolve(&request)? {
        Traced::Resolved(report) => present(&report, request.format).map(CommandOutcome::success),
        Traced::Ambiguous(outcome) => Ok(outcome),
    }
}

/// The daemon-side `trace`: resolve the name and answer from the daemon's own warm session rather
/// than launching a second engine child. The core flow, [`Session::run`], is shared with the fresh
/// path, so the two answers cannot drift.
///
/// Resolution happens after the index wait rather than before it, because the warm resolver can only
/// be trusted once the engine reports its index complete: a half-built index answers with a subset of
/// the declarations, which would change the candidate set rather than merely delay it.
pub(crate) async fn trace_warm(
    engine: &WarmEngine,
    request: TraceRequest<'_>,
) -> Result<CommandOutcome, CommandError> {
    let index = await_warm_index(engine, request.wait).await;
    let candidates = warm_declarations(engine, &request, index).await?;
    match select_candidate(&request, candidates)? {
        Resolution::Ambiguous(outcome) => Ok(outcome),
        Resolution::Ready {
            candidate,
            definition,
        } => {
            let report = Session::new(request.root, request.limit)
                .run(
                    engine.client(),
                    &candidate,
                    definition,
                    request.depth,
                    index,
                )
                .await
                .map_err(CommandError::engine)?;
            present(&report, request.format).map(CommandOutcome::success)
        }
    }
}

/// Every declaration of the requested name, read from the daemon's warm index when that index can
/// answer the name completely and from command-mode `find` when it cannot.
///
/// The warm lookup replaces a `find` subprocess that rebuilds the engine's whole index per
/// invocation, which is most of what a warm trace used to cost. It is not a wider replacement: the
/// engine caps a `workspace/symbol` response, and a name its index holds no declaration for is one
/// `find` answers from its own text-search fallback, so both cases fall back here rather than
/// narrowing the candidate set. The fallback is silent because the answer is the same either way;
/// only its cost differs.
async fn warm_declarations(
    engine: &WarmEngine,
    request: &TraceRequest<'_>,
    index: IndexCompleteness,
) -> Result<Vec<SymbolCandidate>, CommandError> {
    match declarations_from_index(engine.client(), request, index).await {
        Some(candidates) => Ok(candidates),
        None => found_by_command_mode(request).await,
    }
}

/// Every declaration of the requested name the given session's index holds under the root, or `None`
/// when that index cannot answer the name and the caller must ask command-mode `find` instead.
///
/// Shared by the warm and the fresh path so the two resolve a name through the same request against
/// the same kind of index, which is what keeps their candidate sets from drifting. An index short of
/// complete is not asked at all: a half-built one answers with a subset of the declarations, which
/// would change the candidate set rather than merely delay it.
async fn declarations_from_index(
    client: &LspClient,
    request: &TraceRequest<'_>,
    index: IndexCompleteness,
) -> Option<Vec<SymbolCandidate>> {
    if index != IndexCompleteness::Complete {
        return None;
    }
    match ktsense_daemon::resolve_from_warm_index(client, request.root, request.symbol).await {
        WarmResolution::Declarations(candidates) => Some(candidates),
        WarmResolution::Inconclusive(reason) => {
            tracing::debug!(
                "resolving {} through command-mode find: {reason:?}",
                request.symbol
            );
            None
        }
    }
}

pub(crate) fn resolve(request: &TraceRequest<'_>) -> Result<Traced, CommandError> {
    block_on(resolve_in_process(request))
}

/// Resolves the name and, when it is unambiguous, answers from a fresh session. Async so it composes
/// with the async resolver instead of nesting a runtime; [`resolve`] blocks on it for the synchronous
/// callers.
///
/// Command-mode `find` is asked first and unconditionally, which is what keeps an ambiguous name
/// answerable without opening a session at all. Only an empty answer is carried further, because an
/// empty one is the single outcome the engine's cold path can get wrong.
async fn resolve_in_process(request: &TraceRequest<'_>) -> Result<Traced, CommandError> {
    let found = found_by_command_mode(request).await?;
    if found.is_empty() {
        return resolved_against_a_fresh_index(request).await;
    }
    traced(request, select_candidate(request, found)?).await
}

/// The trace for an already-narrowed resolution, answered from a session of its own, or the candidate
/// list to print when the name was ambiguous.
async fn traced(
    request: &TraceRequest<'_>,
    resolution: Resolution,
) -> Result<Traced, CommandError> {
    match resolution {
        Resolution::Ambiguous(outcome) => Ok(Traced::Ambiguous(outcome)),
        Resolution::Ready {
            candidate,
            definition,
        } => {
            let report = collect_fresh(request, &candidate, definition)
                .await
                .map_err(CommandError::engine)?;
            Ok(Traced::Resolved(report))
        }
    }
}

/// A name command-mode `find` reported nothing for, resolved against a fresh session's own index and
/// answered from that same session.
///
/// An empty `find` is not proof of absence. On a cold cache the engine takes its `--fast` path, which
/// is documented as "use rg/fd only" and really does exec `rg`: with no `rg` on `PATH` it searches
/// nothing and writes empty stdout, empty stderr and exit 1, which is byte for byte the shape
/// upstream uses for "matched nothing". Measured on `fixtures/multi-module` from a cold cache: 3
/// declarations with `rg` on `PATH`, 0 without it, 0 with only `fd`, and 3 without either once
/// `kmp-lsp index` has run. GitHub's `ubuntu-24.04` image ships neither tool, which is why the
/// routed and in-process paths disagreed there and nowhere else (KT-67, 2026-09-22).
///
/// Waiting for the index and asking it, rather than re-asking `find`, is the only fix available: the
/// engine's session and its command-mode `find` key their caches differently, so a `find` spawned
/// after the index is complete is still a cold `find`. The session opened here is the one the trace
/// then runs on, so confirming absence costs no extra engine process.
async fn resolved_against_a_fresh_index(
    request: &TraceRequest<'_>,
) -> Result<Traced, CommandError> {
    let (mut client, index) = open_fresh_session(request)
        .await
        .map_err(CommandError::engine)?;
    let traced = indexed_trace(&client, request, index).await;
    let _ = client.shutdown().await;
    traced
}

/// Resolves the name from an open session's index and answers from that same session. An index that
/// cannot answer the name leaves the candidate list empty, so the name is reported absent exactly as
/// it was before this second opinion existed.
async fn indexed_trace(
    client: &LspClient,
    request: &TraceRequest<'_>,
    index: IndexCompleteness,
) -> Result<Traced, CommandError> {
    let candidates = declarations_from_index(client, request, index)
        .await
        .unwrap_or_default();
    match select_candidate(request, candidates)? {
        Resolution::Ambiguous(outcome) => Ok(Traced::Ambiguous(outcome)),
        Resolution::Ready {
            candidate,
            definition,
        } => {
            let report = Session::new(request.root, request.limit)
                .run(client, &candidate, definition, request.depth, index)
                .await
                .map_err(CommandError::engine)?;
            Ok(Traced::Resolved(report))
        }
    }
}

/// What resolving the name settled on: the one declaration to trace, or the candidate list to print
/// when it was ambiguous. Shared by the fresh and warm paths so the ambiguity contract is stated
/// once.
enum Resolution {
    Ready {
        candidate: SymbolCandidate,
        definition: Definition,
    },
    Ambiguous(CommandOutcome),
}

/// Every declaration the engine's command-mode `find` reports for the name. This is the first
/// resolution both paths reach for when they have no settled index, and the fallback a routed trace
/// degrades to when its warm index cannot answer.
async fn found_by_command_mode(
    request: &TraceRequest<'_>,
) -> Result<Vec<SymbolCandidate>, CommandError> {
    ktsense_lsp::run_symbols(request.root, request.symbol)
        .await
        .map_err(|error| CommandError::passthrough(&error))
}

/// Narrows candidates to the one declaration to trace. Both paths route through here, so the
/// ambiguity contract, `--pick`, and the error for a name that matched nothing are stated once and
/// cannot differ between them.
fn select_candidate(
    request: &TraceRequest<'_>,
    candidates: Vec<SymbolCandidate>,
) -> Result<Resolution, CommandError> {
    match symbols::select(
        request.root,
        request.symbol,
        candidates,
        request.pick,
        request.format,
    )? {
        Selection::One(candidate, resolved) => Ok(Resolution::Ready {
            candidate,
            definition: Definition {
                qualified_name: resolved.fqn,
                path: resolved.file,
                line: resolved.line,
                signature: resolved.signature,
            },
        }),
        Selection::Ambiguous(outcome) => Ok(Resolution::Ambiguous(outcome.into())),
    }
}

/// Launches a fresh engine, runs the whole session, and always tears it down, whatever the requests
/// returned.
async fn collect_fresh(
    request: &TraceRequest<'_>,
    candidate: &SymbolCandidate,
    definition: Definition,
) -> Result<TraceReport, LspError> {
    let (mut client, index) = open_fresh_session(request).await?;
    let outcome = Session::new(request.root, request.limit)
        .run(&client, candidate, definition, request.depth, index)
        .await;
    let _ = client.shutdown().await;
    outcome
}

/// A fresh engine session with its index waited out, and the completeness that wait reached. The wait
/// happens here because a fresh session must build its index before it can answer anything; the warm
/// path reads the daemon's already-tracked phase instead. The caller owns the teardown, so one
/// session can both resolve a name and answer about it.
async fn open_fresh_session(
    request: &TraceRequest<'_>,
) -> Result<(LspClient, IndexCompleteness), LspError> {
    let mut client = ktsense_lsp::launch().await?;
    let config = InitializeConfig {
        root_uri: format!("file://{}", canonical(request.root).display()),
        ignore_patterns: vec![IGNORED_BUILD_OUTPUT.to_string()],
    };
    client.initialize(&config).await?;
    let wait = wait_for_index(&mut client, request.wait.cap()).await;
    Ok((client, completeness(wait.phase)))
}

/// The index completeness the daemon's warm session can honestly claim, waiting under the same
/// policy a fresh session would: until the tracked phase is ready, until the engine's progress
/// stream closes, or until the cap elapses, whichever comes first. A stream that closes before
/// `Ready` ends the wait at once with the phase reached, so an `--wait-index` request whose engine
/// notifications stop cannot spin for the day-long cap; a `Ready` already observed is reported
/// complete even if the stream then closes. The phase is polled from the tracker rather than
/// drained from the notification stream, which the daemon already consumes.
async fn await_warm_index(index: &impl IndexLifecycle, wait: IndexWaitPolicy) -> IndexCompleteness {
    let deadline = tokio::time::Instant::now() + wait.cap();
    loop {
        let phase = index.phase();
        if phase.is_ready() {
            return IndexCompleteness::Complete;
        }
        if index.closed() {
            return completeness(index.phase());
        }
        if tokio::time::Instant::now() >= deadline {
            return completeness(phase);
        }
        tokio::time::sleep(WARM_INDEX_POLL).await;
    }
}

/// The narrow view of the daemon's index lifecycle [`await_warm_index`] needs: the phase last
/// observed and whether the progress stream has closed. Naming it lets the wait be exercised
/// against a hand-built lifecycle without launching an engine.
trait IndexLifecycle {
    fn phase(&self) -> IndexPhase;
    fn closed(&self) -> bool;
}

impl IndexLifecycle for WarmEngine {
    fn phase(&self) -> IndexPhase {
        self.index_phase()
    }

    fn closed(&self) -> bool {
        self.index_closed()
    }
}

/// How often [`await_warm_index`] rechecks the tracker. Short enough that a just-finished index is
/// noticed promptly, long enough that the poll costs nothing next to the engine requests.
const WARM_INDEX_POLL: Duration = Duration::from_millis(20);

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

    /// The shared core flow: ask the engine for implementors and references at the declaration,
    /// build the report at the given index completeness, then follow callers for each further level
    /// of depth. `client` is a fresh session on the in-process path and the daemon's warm session on
    /// the routed path, so the two answers are identical by construction.
    async fn run(
        mut self,
        client: &LspClient,
        candidate: &SymbolCandidate,
        definition: Definition,
        depth: usize,
        index: IndexCompleteness,
    ) -> Result<TraceReport, LspError> {
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
            index,
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

    /// Where to ask the engine about the resolved declaration: the candidate's own position, which is
    /// already the declaration name's 1-based character column rather than the engine's raw one.
    ///
    /// Both producers relocate it before it arrives, `ktsense_lsp::SymbolResolver::find` for
    /// command mode and the daemon's warm lookup for a routed trace, each against the column its own
    /// engine answer reported. That is what makes the two paths agree, and it is also the only place
    /// the reported column exists: relocating again here would have to do it without one and would
    /// take the leftmost occurrence, which on a line like
    /// `public value class TypeOfService(public val value: UByte)` is the soft keyword rather than the
    /// declared property. The engine's own column is not trusted directly either: on a cold cache its
    /// command-mode `find` takes a text-search path and has been observed to report the column of the
    /// keyword before the name (`val CallLogging` at the `v`), and a references request there answers
    /// with every use of the keyword, 14,702 sites on ktor instead of 31.
    fn declaration_position(&self, candidate: &SymbolCandidate) -> FilePosition {
        candidate.to_file_position()
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
///
/// Used for a caller declaration reached by walking the report, which the engine never located by
/// itself, so there is no reported column to disambiguate a name that appears twice on that line and
/// the leftmost occurrence is the only available answer. A resolved candidate carries its own
/// relocated column and goes through [`Session::declaration_position`] instead.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built index lifecycle, so the wait can be exercised without launching an engine.
    struct ClosedLifecycle {
        phase: IndexPhase,
    }

    impl IndexLifecycle for ClosedLifecycle {
        fn phase(&self) -> IndexPhase {
            self.phase
        }

        fn closed(&self) -> bool {
            true
        }
    }

    /// The wait must never approach [`UNBOUNDED_WAIT`]; a few seconds is generous for a poll that
    /// should return on its first iteration.
    const TEST_DEADLINE: Duration = Duration::from_secs(5);

    async fn await_closed(
        phase: IndexPhase,
    ) -> Result<IndexCompleteness, tokio::time::error::Elapsed> {
        tokio::time::timeout(
            TEST_DEADLINE,
            await_warm_index(&ClosedLifecycle { phase }, IndexWaitPolicy::UntilReady),
        )
        .await
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_unbounded_wait_promptly_at_the_final_phase() {
        let still_indexing = await_closed(IndexPhase::Indexing).await;
        let already_ready = await_closed(IndexPhase::Ready).await;

        assert_eq!(
            (still_indexing, already_ready),
            (
                Ok(IndexCompleteness::Partial),
                Ok(IndexCompleteness::Complete)
            )
        );
    }
}
