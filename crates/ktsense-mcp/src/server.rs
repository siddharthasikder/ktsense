//! The rmcp server: eight tools, each an invocation of the `ktsense` binary through a [`Runner`].

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{
    tool, tool_handler, tool_router, ErrorData, Peer, RoleServer, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::citations::{index_answer, Answer, Call};
use crate::roots::ClientRoots;
use crate::warm::{EngineWarmer, WarmEngines, Warmth};
use crate::Tool;

/// Exit codes the CLI documents as answers rather than failures: success, and an ambiguous name,
/// whose output is the candidate list an agent picks from.
const ANSWER_EXITS: [i32; 2] = [0, 3];

/// What one command invocation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The workspace root the invocation actually answered about, reported by whatever ran it so a
    /// result can state it rather than the tool layer guessing which root won.
    pub root: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("could not run ktsense at {binary}: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
}

/// Output format a request asks the CLI for. Answers are Markdown, because that is what an agent
/// reads; the server's own probes ask for JSON so they parse a field instead of matching prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Md,
    Json,
}

impl Format {
    fn flag(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Json => "json",
        }
    }
}

/// One command to run: the arguments after the global flags, the root to answer about when the
/// call names one, and the format to render. The runner owns the default root, so a call's root
/// replaces it rather than being added alongside, which the CLI would refuse as a repeated flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub args: Vec<String>,
    pub root: Option<String>,
    pub format: Format,
}

/// The port through which tools reach the commands. The product adapter runs the binary; tests
/// record the requests and reply with a scripted invocation.
pub trait Runner: Send + Sync {
    fn run(
        &self,
        request: Request,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Invocation, RunnerError>> + Send + '_>,
    >;

    /// The root this runner answers about when a request names none, which is what the server has to
    /// know to keep the right workspace warm. The runner owns that default, so it reports it rather
    /// than the server holding a second copy that could disagree.
    fn default_root(&self) -> PathBuf;
}

/// Runs the `ktsense` binary at `binary` with the configured default root.
#[derive(Debug, Clone)]
pub struct ExecutableRunner {
    binary: PathBuf,
    root: PathBuf,
}

impl ExecutableRunner {
    pub fn new(binary: PathBuf, root: PathBuf) -> Self {
        Self { binary, root }
    }
}

impl Runner for ExecutableRunner {
    fn run(
        &self,
        request: Request,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Invocation, RunnerError>> + Send + '_>,
    > {
        Box::pin(async move {
            let root = request
                .root
                .map(PathBuf::from)
                .unwrap_or_else(|| self.root.clone());
            let output = tokio::process::Command::new(&self.binary)
                .arg("--root")
                .arg(&root)
                .arg("--format")
                .arg(request.format.flag())
                .args(&request.args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|source| RunnerError::Spawn {
                    binary: self.binary.display().to_string(),
                    source,
                })?;
            Ok(Invocation {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                root: root.display().to_string(),
            })
        })
    }

    fn default_root(&self) -> PathBuf {
        self.root.clone()
    }
}

/// How the server finds the binary it delegates to and which workspace it answers about by
/// default. The CLI passes its own executable path.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub binary: PathBuf,
    pub root: PathBuf,
}

/// Set to `1` to stop the server holding a warm engine of its own.
///
/// Warming pays for itself on a repository large enough that the engine's text-search fallback is
/// wrong, and costs about half a second for nothing on a tree small enough that it is right: KT-33
/// measured 530 ms of pure cost on a nine-file fixture. This is the switch for that case, and for an
/// operator who wants the MCP server to spawn no engine but the ones its commands spawn themselves.
pub const NO_WARM_ENGINE_ENV: &str = "KTSENSE_MCP_NO_WARM_ENGINE";

/// Serves the tools over stdio until the client disconnects, holding a warm engine for any root
/// whose index-shaped tools get used and that no daemon is already keeping warm.
pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    let runner = ExecutableRunner::new(config.binary, config.root);
    let engines =
        warming_enabled().then(|| Arc::new(WarmEngines::new(Arc::new(EngineWarmer::default()))));
    let server = match &engines {
        Some(engines) => KtsenseServer::warming(Arc::new(runner), engines.clone()),
        None => KtsenseServer::new(Arc::new(runner)),
    };
    let running = server.serve(rmcp::transport::stdio()).await?;
    let outcome = running.waiting().await;
    if let Some(engines) = engines {
        engines.close().await;
    }
    outcome?;
    Ok(())
}

fn warming_enabled() -> bool {
    std::env::var_os(NO_WARM_ENGINE_ENV).is_none_or(|value| value != "1")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineParams {
    /// Path to one Kotlin file, absolute or relative to the workspace root.
    pub file: String,
    /// Include private and internal declarations, hidden by default.
    #[serde(default)]
    pub private: bool,
    /// Include the first line of each declaration's KDoc.
    #[serde(default)]
    pub kdoc: bool,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolParams {
    /// Declaration name to find, for example `OrderRepository` or `save`.
    pub query: String,
    /// Keep only declarations of this kind: class, interface, object, fun, val, var, typealias
    /// or constructor.
    pub kind: Option<String>,
    /// Show at most this many rows when the name is ambiguous.
    pub limit: Option<usize>,
    /// Select the single candidate with this fully-qualified name.
    pub pick: Option<String>,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceParams {
    /// Declaration name to trace.
    pub symbol: String,
    /// Select the single candidate with this fully-qualified name when the name is ambiguous.
    pub pick: Option<String>,
    /// How many levels of callers to follow, 1 to 5; 1 is the declarations that refer to it.
    pub depth: Option<u8>,
    /// Show at most this many reference sites per file.
    pub limit: Option<usize>,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DepsParams {
    /// Whether nodes are `package` (default) or `file`.
    pub level: Option<String>,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MapParams {
    /// Token budget for the map; defaults to 4000.
    pub budget: Option<usize>,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckParams {
    /// A Kotlin file or a directory to syntax-check.
    pub path: String,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextParams {
    /// Declaration name to explain.
    pub symbol: String,
    /// Select the single candidate with this fully-qualified name when the name is ambiguous.
    pub pick: Option<String>,
    /// Token budget for the bundle; defaults to 2000.
    pub budget: Option<usize>,
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Workspace root to answer about; defaults to the server's configured root.
    pub root: Option<String>,
}

/// The JSON Schema every tool declares for its `structuredContent`, so a client that validates
/// structured results has something to validate against.
fn answer_schema() -> Arc<JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<Answer>()
        .expect("Answer is a struct, so its schema is a JSON object")
}

/// How long the server waits for a client to answer `roots/list` before deciding it has no usable
/// roots. Generous, since the client may be prompting a person, and finite so a client that
/// advertises the capability and then never answers cannot wedge every tool call.
const ROOTS_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// The MCP server. Every tool builds a CLI argument list and hands it to the runner.
#[derive(Clone)]
pub struct KtsenseServer {
    runner: Arc<dyn Runner>,
    tool_router: ToolRouter<Self>,
    /// The roots the client advertised, fetched once and cleared when it says they changed. `None`
    /// means not asked yet; an empty `ClientRoots` means asked, and there are none to use.
    client_roots: Arc<RwLock<Option<ClientRoots>>>,
    warm: Option<Arc<WarmEngines>>,
}

impl KtsenseServer {
    /// A server that answers tool calls and holds no warm engine, which is what the unit tests and
    /// the catalogue snapshot need.
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            tool_router: Self::tool_router(),
            client_roots: Arc::new(RwLock::new(None)),
            warm: None,
        }
    }

    /// A server that additionally keeps an engine warm for the roots whose index-dependent tools get
    /// used, unless a daemon is already holding one.
    pub fn warming(runner: Arc<dyn Runner>, engines: Arc<WarmEngines>) -> Self {
        Self {
            warm: Some(engines),
            ..Self::new(runner)
        }
    }

    /// The tools as the client will list them, for tests that check the catalogue is honoured.
    pub fn listed_tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// The root a call is about: its own argument, else the advertised root holding `subject`, else
    /// `None` so the runner uses the root the server was started with.
    async fn root_for(&self, call: Option<String>, subject: Option<&str>) -> Option<String> {
        let advertised = self.client_roots.read().await.clone().unwrap_or_default();
        advertised.resolve(call, subject)
    }

    /// Asks the client for its roots, once. Called before every dispatch rather than from the
    /// `initialized` notification, so the answer is in hand before any tool body reads it instead of
    /// racing a notification handler.
    async fn learn_roots(&self, peer: &Peer<RoleServer>) {
        if self.client_roots.read().await.is_some() {
            return;
        }
        let advertised = ClientRoots::new(advertised_roots(peer).await);
        self.client_roots.write().await.replace(advertised);
    }

    /// Makes sure an engine is warm for the root this call is about, when the index would shape the
    /// answer. Never fails the call: a root that could not be warmed is answered anyway, just from a
    /// cold index.
    async fn keep_warm(&self, tool: &Tool, request: &Request) -> Option<Warmth> {
        if !tool.index_shapes_answer {
            return None;
        }
        let engines = self.warm.as_ref()?;
        let root = request
            .root
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.runner.default_root());
        let warmth = engines
            .ensure(&root, || self.daemon_holds(root.clone()))
            .await;
        tracing::debug!(root = %root.display(), ?warmth, "engine warmth for this root");
        Some(warmth)
    }

    /// Whether a daemon is already holding a warm session for `root`, asked of our own `status`
    /// command in JSON. The state is read from a field rather than matched in prose: `no daemon
    /// running` contains `daemon running`, and KT-38 records a benchmark that believed a daemon was
    /// live for exactly that reason.
    async fn daemon_holds(&self, root: PathBuf) -> bool {
        let probe = Request {
            args: vec![crate::STATUS.cli_command.to_string()],
            root: Some(root.display().to_string()),
            format: Format::Json,
        };
        let Ok(invocation) = self.runner.run(probe).await else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&invocation.stdout)
            .ok()
            .and_then(|report| report["daemon"]["state"].as_str().map(str::to_string))
            .is_some_and(|state| state == "running")
    }

    async fn invoke(&self, tool: &Tool, request: Request) -> Result<CallToolResult, ErrorData> {
        let warmth = self.keep_warm(tool, &request).await;
        let invocation = self
            .runner
            .run(request)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(present(tool, invocation, warmth))
    }
}

/// The client's roots, or none when it never advertised the capability, declined, or did not answer
/// within [`ROOTS_BOUND`].
///
/// `Peer::list_roots` is deprecated in rmcp 2.2.0 because SEP-2577 deprecates roots protocol-wide,
/// but it is still what every client that has roots speaks, and answering about the wrong workspace
/// is the failure this prevents. The allow is scoped to this one call so the deprecation stays
/// visible everywhere else.
#[allow(deprecated)]
async fn advertised_roots(peer: &Peer<RoleServer>) -> Vec<PathBuf> {
    let declared = peer
        .peer_info()
        .is_some_and(|info| info.capabilities.roots.is_some());
    if !declared {
        return Vec::new();
    }
    match tokio::time::timeout(ROOTS_BOUND, peer.list_roots()).await {
        Ok(Ok(listed)) => listed
            .roots
            .iter()
            .map(|root| ktsense_lsp::uri_to_path(&root.uri))
            .collect(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "client declared roots but could not list them");
            Vec::new()
        }
        Err(_elapsed) => {
            tracing::warn!(bound = ?ROOTS_BOUND, "client did not answer roots/list");
            Vec::new()
        }
    }
}

/// An exit the CLI documents as an answer becomes the answer text; anything else becomes a tool
/// error carrying the command's own message, so the agent sees `no declaration named X` rather
/// than a bare failure. Either way the result carries its citation index, because the files a
/// failure names are as worth following as the ones an answer names.
fn present(tool: &Tool, invocation: Invocation, warmth: Option<Warmth>) -> CallToolResult {
    let answered = invocation
        .code
        .is_some_and(|code| ANSWER_EXITS.contains(&code));
    let text = if answered {
        invocation.stdout
    } else if invocation.stderr.trim().is_empty() {
        invocation.stdout.trim().to_string()
    } else {
        invocation.stderr.trim().to_string()
    };
    let indexed = index_answer(
        Call {
            tool,
            root: &invocation.root,
            exit: invocation.code,
            warmth: warmth.map(Warmth::label),
        },
        &text,
    );
    let content = vec![ContentBlock::text(text)];
    let mut result = if answered {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    result.structured_content = serde_json::to_value(&indexed).ok();
    result
}

/// What a `check` invocation actually was, which its exit status alone cannot say: the CLI exits 1
/// both on a file with syntax errors and on a failure to run the engine, so KT-31's map of "exit 1
/// is a tool error" hid a real answer behind the same status as a missing engine.
///
/// The distinction is read from the CLI's own stream discipline, enforced by its `main`: an answer
/// is written to stdout and any diagnostic to stderr. A syntax finding therefore arrives as a
/// report on stdout with an empty stderr under exit 1, while every `PassthroughError` (a missing,
/// timed-out, or unparseable engine) arrives as a message on stderr with an empty stdout under the
/// same exit. This keys on which stream carried the output, not on matching words in it, and it
/// leaves the CLI's exit codes untouched so a shell caller still branches on them as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckOutcome {
    Clean,
    Findings,
    ExecutionFailure,
}

impl CheckOutcome {
    fn classify(invocation: &Invocation) -> Self {
        match invocation.code {
            Some(0) => CheckOutcome::Clean,
            Some(1)
                if !invocation.stdout.trim().is_empty() && invocation.stderr.trim().is_empty() =>
            {
                CheckOutcome::Findings
            }
            _ => CheckOutcome::ExecutionFailure,
        }
    }

    fn label(self) -> &'static str {
        match self {
            CheckOutcome::Clean => "clean",
            CheckOutcome::Findings => "findings",
            CheckOutcome::ExecutionFailure => "execution_failure",
        }
    }

    fn is_answer(self) -> bool {
        !matches!(self, CheckOutcome::ExecutionFailure)
    }
}

/// Presents a `check` invocation, which unlike every other tool can exit non-zero and still have
/// answered. A finding or a clean file is a successful result carrying the report; only a failure
/// to run the engine is a tool error. The structured half names the outcome and counts the cited
/// error sites so an agent branches on a field rather than on `isError` alone.
fn present_check(invocation: Invocation) -> CallToolResult {
    let outcome = CheckOutcome::classify(&invocation);
    let text = if outcome.is_answer() {
        invocation.stdout
    } else if invocation.stderr.trim().is_empty() {
        invocation.stdout.trim().to_string()
    } else {
        invocation.stderr.trim().to_string()
    };
    let mut answer = index_answer(
        Call {
            tool: &crate::CHECK,
            root: &invocation.root,
            exit: invocation.code,
            warmth: None,
        },
        &text,
    );
    answer.outcome = Some(outcome.label().to_string());
    answer.findings = match outcome {
        CheckOutcome::Clean => Some(0),
        CheckOutcome::Findings => Some(answer.citations.len() + answer.citations_omitted),
        CheckOutcome::ExecutionFailure => None,
    };
    let content = vec![ContentBlock::text(text)];
    let mut result = if outcome.is_answer() {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    result.structured_content = serde_json::to_value(&answer).ok();
    result
}

/// Builds the `<command> <args...>` part of a request for one catalogued tool.
struct Args(Request);

impl Args {
    fn for_tool(tool: &Tool, root: Option<String>) -> Self {
        Self(Request {
            args: vec![tool.cli_command.to_string()],
            root,
            format: Format::Md,
        })
    }

    fn positional(mut self, value: impl Into<String>) -> Self {
        self.0.args.push(value.into());
        self
    }

    fn flag(mut self, name: &str, present: bool) -> Self {
        if present {
            self.0.args.push(format!("--{name}"));
        }
        self
    }

    fn option<T: ToString>(mut self, name: &str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.0.args.push(format!("--{name}"));
            self.0.args.push(value.to_string());
        }
        self
    }
}

#[tool_router]
impl KtsenseServer {
    #[tool(
        name = "get_kotlin_outline",
        description = "Answers what one Kotlin file declares: every signature with no bodies, public API only unless you ask for private. Prefer it over reading the file whenever you want the shape rather than the implementation; it is the cheapest tool here. requires: nothing. cost: about 30 ms on a 54 KB file (ktor 3.0.1, median of 9, KT-38).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn get_kotlin_outline(
        &self,
        Parameters(params): Parameters<OutlineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, Some(&params.file)).await;
        let args = Args::for_tool(&crate::OUTLINE, root)
            .positional(params.file)
            .flag("private", params.private)
            .flag("kdoc", params.kdoc);
        self.invoke(&crate::OUTLINE, args.0).await
    }

    #[tool(
        name = "find_kotlin_symbol",
        description = "Answers where a name is declared: kind, file, line and signature for every declaration carrying it. Prefer it over grep, which also finds uses and cannot tell a class from a local. Several matches come back as a list to choose from; pass pick with one fully-qualified name to narrow. Against an unsettled index the engine falls back to a text search and one declaration can come back as several candidates, so check ktsense_status when a result looks duplicated. requires: kmp-lsp. cost: about 300 ms on 1861 files (ktor 3.0.1, median of 9, KT-38).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn find_kotlin_symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::SYMBOLS, root)
            .positional(params.query)
            .option("kind", params.kind)
            .option("limit", params.limit)
            .option("pick", params.pick);
        self.invoke(&crate::SYMBOLS, args.0).await
    }

    #[tool(
        name = "trace_kotlin_symbol",
        description = "Answers who uses one declaration: its definition, its implementors, the declarations that call it, and every reference site grouped by file. Prefer it over grep for who-calls and who-implements, which grep cannot separate from a definition. Every answer states index: complete or partial, and partial is a lower bound rather than the answer. requires: kmp-lsp and a settled index. cost: about 1.2 s on 1861 files (ktor 3.0.1, median of 9, KT-38).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn trace_kotlin_symbol(
        &self,
        Parameters(params): Parameters<TraceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::TRACE, root)
            .positional(params.symbol)
            .option("pick", params.pick)
            .option("depth", params.depth)
            .option("limit", params.limit);
        self.invoke(&crate::TRACE, args.0).await
    }

    #[tool(
        name = "analyze_kotlin_dependencies",
        description = "Answers which packages or files import which, listing the cycles and the external imports as well. Prefer it over grepping import lines when you want the whole graph or the cycles in it. Edges are import statements, not type-checked references. requires: nothing. cost: about 1.7 s on 1861 files, the slowest tool here because it parses every file in the tree (ktor 3.0.1, median of 9, KT-38).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn analyze_kotlin_dependencies(
        &self,
        Parameters(params): Parameters<DepsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::DEPS, root).option("level", params.level);
        self.invoke(&crate::DEPS, args.0).await
    }

    #[tool(
        name = "get_kotlin_repo_map",
        description = "Answers what a repository is: its most central files first with their signatures, packed into a token budget. Start here on an unfamiliar repository, before reaching for any file-level tool. Test sources are excluded and ranking is by import centrality, which is syntactic. requires: nothing. cost: about 1.1 s on 1861 files (ktor 3.0.1, median of 9, KT-38), so call it once and keep the answer instead of re-asking.",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn get_kotlin_repo_map(
        &self,
        Parameters(params): Parameters<MapParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::MAP, root).option("budget", params.budget);
        self.invoke(&crate::MAP, args.0).await
    }

    #[tool(
        name = "check_kotlin_syntax",
        description = "Answers whether a file or a directory parses, reporting every syntax error with its position. Run it after each edit. It is syntax only: type errors are the compiler's job and are not reported. A file with syntax errors is a successful result whose report lists each error and its position, not a tool error; only a failure to run the engine (missing, timed out) is an error result, so branch on the structured outcome rather than on isError alone. requires: kmp-lsp, which has to be installed even though no index is waited for. cost: about 145 ms for one 54 KB file (ktor 3.0.1, median of 9, KT-32).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn check_kotlin_syntax(
        &self,
        Parameters(params): Parameters<CheckParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, Some(&params.path)).await;
        let args = Args::for_tool(&crate::CHECK, root).positional(params.path);
        let invocation = self
            .runner
            .run(args.0)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(present_check(invocation))
    }

    #[tool(
        name = "explain_kotlin_symbol",
        description = "Answers everything worth knowing about one symbol in a single budgeted bundle: its declaration, the outline of its file, its direct callers and its implementors, trimmed in that order of priority. Prefer it over calling find, outline and trace separately when you are orienting yourself around an unfamiliar symbol; reach for trace_kotlin_symbol instead when you need every reference site or callers deeper than one level. An ambiguous name comes back as the candidate list; pass pick with one fully-qualified name to choose. Carries the same index: complete or partial marker a trace does. requires: kmp-lsp and a settled index. cost: about 1.1 s on 1861 files (ktor 3.0.1, median of 9, KT-34).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn explain_kotlin_symbol(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::CONTEXT, root)
            .positional(params.symbol)
            .option("pick", params.pick)
            .option("budget", params.budget);
        self.invoke(&crate::CONTEXT, args.0).await
    }

    #[tool(
        name = "ktsense_status",
        description = "Answers what this installation can do right now: the workspace root, how many Kotlin files it holds, whether kmp-lsp is present and version-compatible, and whether a warm daemon is holding an index. Call it first when another tool fails, and before a trace when you need to know whether the index has settled. A missing engine is reported rather than raised. requires: nothing. cost: about 100 ms on 1990 Kotlin files (ktor 3.0.1, median of 9, KT-32).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn ktsense_status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root_for(params.root, None).await;
        let args = Args::for_tool(&crate::STATUS, root);
        self.invoke(&crate::STATUS, args.0).await
    }
}

/// What the client is told before it reads a single tool description. Everything here is true of
/// every tool, and the version-check scope is stated because it is not uniform: only the tool that
/// opens an LSP session refuses an incompatible engine.
const INSTRUCTIONS: &str = "Kotlin code understanding for agents.\n\n\
     Every tool states what it requires. `requires: nothing` is answered by ktsense alone with \
     tree-sitter, so it works on a host with no engine installed. `requires: kmp-lsp` needs the \
     engine binary present but waits for no index. `requires: kmp-lsp and a settled index` needs \
     both, and those answers carry an `index:` marker whose `partial` means the list is a lower \
     bound, not the answer. Requiring nothing is not the same as being cheap: the tools that walk \
     the whole tree take about a second on a 1861-file repository, and each description carries its \
     measured cost.\n\n\
     Call `ktsense_status` to learn whether the engine is installed and version-compatible. Only \
     `trace_kotlin_symbol` refuses to run against an incompatible engine version, because it is the \
     one tool that opens an LSP session; `find_kotlin_symbol` and `check_kotlin_syntax` use the \
     engine's command mode, which is not version-guarded.\n\n\
     Every result carries the answer as Markdown text plus `structuredContent` listing the files \
     and lines that answer cites, so following a result up needs no parsing of the prose.\n\n\
     Resolution is syntactic, never type-checked. A result is a candidate, not a proof, and type \
     errors are the compiler's job.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KtsenseServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ktsense", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    /// Learns the client's roots before dispatching, so a tool body reads an answer that is already
    /// in hand. Doing it here rather than from the `initialized` notification is what makes the
    /// choice of root deterministic: notifications are handled in their own task, which a tool call
    /// arriving straight afterwards can overtake.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.learn_roots(&context.peer).await;
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(call).await
    }

    /// The client says its roots changed, so the cached answer is dropped and the next call asks
    /// again. A root list that went stale would send answers to the wrong workspace.
    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {
        self.client_roots.write().await.take();
    }
}

impl std::fmt::Debug for KtsenseServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KtsenseServer").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{json, Value};

    use super::*;
    use crate::TOOLS;

    /// Replies with a scripted invocation and records the arguments each call produced.
    struct Recorded {
        reply: Invocation,
        calls: Mutex<Vec<Request>>,
    }

    impl Recorded {
        fn replying(code: i32, stdout: &str, stderr: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: Invocation {
                    code: Some(code),
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                    root: "/repo".to_string(),
                },
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl Runner for Recorded {
        fn run(
            &self,
            request: Request,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Invocation, RunnerError>> + Send + '_>,
        > {
            self.calls.lock().unwrap().push(request);
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }

        fn default_root(&self) -> PathBuf {
            PathBuf::from("/repo")
        }
    }

    fn catalogued(name: &str) -> &'static Tool {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("listed tools come from the catalogue")
    }

    /// What an answer says the server did about warmth, which is how a caller with no `index:` marker
    /// weighs the result.
    fn warmth(result: &CallToolResult) -> Option<Value> {
        result
            .structured_content
            .as_ref()
            .map(|structured| structured["warmth"].clone())
    }

    /// Whether a description states a cost the way KT-32 requires: the marker, and then either a
    /// figure or an explicit admission that nothing was measured.
    ///
    /// `cost: fast` is the case this exists to reject. It is what KT-31 shipped, it reads as cheap, and
    /// KT-38 measured `analyze_kotlin_dependencies` at 1.7 s and `get_kotlin_repo_map` at 1.1 s. An
    /// admission is allowed because the alternative would be a test that pressures the next author into
    /// inventing a number for a tool nobody has timed.
    fn states_a_cost(description: &str) -> bool {
        let Some(stated) = description.split("cost: ").nth(1) else {
            return false;
        };
        let stated = stated.trim().trim_end_matches('.').trim();
        stated.contains(|character: char| character.is_ascii_digit()) || stated == "unmeasured"
    }

    #[test]
    fn a_cost_marker_needs_a_figure_or_an_admission_and_fast_is_neither() {
        let judged = [
            "requires: nothing. cost: about 30 ms on a 54 KB file (ktor 3.0.1, median of 9, KT-38).",
            "requires: nothing. cost: unmeasured.",
            "requires: nothing. cost: fast (no index).",
            "requires: nothing. cost: .",
            "requires: nothing.",
        ]
        .map(states_a_cost);

        assert_eq!(judged, [true, true, false, false, false]);
    }

    fn is_object_schema(schema: &Value) -> bool {
        schema.get("type") == Some(&json!("object")) && schema.get("properties").is_some()
    }

    #[test]
    fn every_listed_tool_is_catalogued_read_only_and_states_its_requirement_and_its_cost() {
        let server = KtsenseServer::new(Recorded::replying(0, "", ""));
        let listed = server.listed_tools();

        let mut names: Vec<String> = listed.iter().map(|tool| tool.name.to_string()).collect();
        names.sort();
        let mut catalogued_names: Vec<String> =
            TOOLS.iter().map(|tool| tool.name.to_string()).collect();
        catalogued_names.sort();
        let schemas_valid = listed.iter().all(|tool| {
            is_object_schema(&Value::Object((*tool.input_schema).clone()))
                && tool.output_schema.as_ref().is_some_and(|schema| {
                    is_object_schema(&Value::Object((**schema).clone()))
                        && Some(schema) == listed[0].output_schema.as_ref()
                })
        });
        let described = listed.iter().all(|tool| {
            let description = tool.description.as_deref().unwrap_or_default();
            let entry = catalogued(&tool.name);
            description.contains(&format!("requires: {}", entry.requires.label()))
                && states_a_cost(description)
        });
        let read_only = listed.iter().all(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
        });

        assert_eq!(
            (listed.len(), names, schemas_valid, described, read_only),
            (8, catalogued_names, true, true, true)
        );
    }

    #[tokio::test]
    async fn a_tool_call_builds_the_request_with_its_root_override_and_returns_stdout_as_the_answer(
    ) {
        let runner = Recorded::replying(
            0,
            "# Trace: save\n\nindex: complete\n\n## Definition\n\ncore/Repo.kt:4\n",
            "",
        );
        let server = KtsenseServer::new(runner.clone());

        let result = server
            .trace_kotlin_symbol(Parameters(TraceParams {
                symbol: "save".to_string(),
                pick: Some("shop.order.OrderRepository.save".to_string()),
                depth: Some(2),
                limit: None,
                root: Some("/repo".to_string()),
            }))
            .await
            .expect("tool call");

        let observed = (
            runner.calls.lock().unwrap().clone(),
            result.is_error,
            serde_json::to_value(&result.content).expect("json")[0]["text"].clone(),
            result.structured_content.clone(),
        );
        assert_eq!(
            observed,
            (
                vec![Request {
                    args: vec![
                        "trace".to_string(),
                        "save".to_string(),
                        "--pick".to_string(),
                        "shop.order.OrderRepository.save".to_string(),
                        "--depth".to_string(),
                        "2".to_string(),
                    ],
                    root: Some("/repo".to_string()),
                    format: Format::Md,
                }],
                Some(false),
                json!("# Trace: save\n\nindex: complete\n\n## Definition\n\ncore/Repo.kt:4\n"),
                Some(json!({
                    "tool": "trace_kotlin_symbol",
                    "command": "trace",
                    "requires": "kmp-lsp and a settled index",
                    "root": "/repo",
                    "exit": 0,
                    "index": "complete",
                    "files": ["core/Repo.kt"],
                    "citations": [{ "path": "core/Repo.kt", "line": 4 }],
                    "citations_omitted": 0,
                })),
            )
        );
    }

    #[tokio::test]
    async fn an_ambiguous_exit_is_an_answer_and_a_failure_exit_is_a_tool_error_that_still_indexes()
    {
        let ambiguous = KtsenseServer::new(Recorded::replying(3, "## Symbols: save\n", ""));
        let failed = KtsenseServer::new(Recorded::replying(
            1,
            "",
            "ktsense: no declaration named ZzzNope\n",
        ));
        let params = |query: &str| {
            Parameters(SymbolParams {
                query: query.to_string(),
                kind: None,
                limit: None,
                pick: None,
                root: None,
            })
        };

        let listed = ambiguous
            .find_kotlin_symbol(params("save"))
            .await
            .expect("call");
        let missing = failed
            .find_kotlin_symbol(params("ZzzNope"))
            .await
            .expect("call");

        let text = |result: &CallToolResult| {
            serde_json::to_value(&result.content).expect("json")[0]["text"].clone()
        };
        let exit = |result: &CallToolResult| {
            result
                .structured_content
                .as_ref()
                .map(|structured| structured["exit"].clone())
        };
        assert_eq!(
            (
                listed.is_error,
                text(&listed),
                exit(&listed),
                missing.is_error,
                text(&missing),
                exit(&missing),
            ),
            (
                Some(false),
                json!("## Symbols: save\n"),
                Some(json!(3)),
                Some(true),
                json!("ktsense: no declaration named ZzzNope"),
                Some(json!(1)),
            )
        );
    }

    /// The KT-57 distinction, proven at the `present` boundary the runner feeds: a `check` that ran
    /// is an answer whether the file was clean or broken, and a `check` that could not reach the
    /// engine is a tool error, even though the CLI exits 1 for a broken file and for a missing
    /// engine alike. Each case is the exact stream shape the CLI's `main` produces: a report on
    /// stdout for an answer, a `PassthroughError` on stderr for a failure.
    #[test]
    fn check_findings_and_a_clean_file_are_answers_while_a_failure_to_run_the_engine_is_an_error() {
        let clean = Invocation {
            code: Some(0),
            stdout: "## Syntax check\n\n1 OK, 0 with errors.\n".to_string(),
            stderr: String::new(),
            root: "/repo".to_string(),
        };
        let findings = Invocation {
            code: Some(1),
            stdout: "## Syntax check\n\n0 OK, 1 with errors.\n\n```text\nsrc/Broken.kt:12:5: unexpected `fun`\n```\n".to_string(),
            stderr: String::new(),
            root: "/repo".to_string(),
        };
        let missing_engine = Invocation {
            code: Some(1),
            stdout: String::new(),
            stderr:
                "ktsense: failed to spawn engine `kmp-lsp`: No such file or directory (os error 2)"
                    .to_string(),
            root: "/repo".to_string(),
        };
        let timed_out = Invocation {
            code: Some(1),
            stdout: String::new(),
            stderr: "ktsense: engine `check` did not respond within 60s".to_string(),
            root: "/repo".to_string(),
        };

        let observed: Vec<(Option<bool>, Value, Value, Value)> =
            [clean, findings, missing_engine, timed_out]
                .into_iter()
                .map(|invocation| {
                    let result = present_check(invocation);
                    let structured = result.structured_content.clone().unwrap_or(Value::Null);
                    (
                        result.is_error,
                        structured["outcome"].clone(),
                        structured["findings"].clone(),
                        structured["citations"].clone(),
                    )
                })
                .collect();

        assert_eq!(
            observed,
            vec![
                (Some(false), json!("clean"), json!(0), json!([])),
                (
                    Some(false),
                    json!("findings"),
                    json!(1),
                    json!([{ "path": "src/Broken.kt", "line": 12, "column": 5 }]),
                ),
                (
                    Some(true),
                    json!("execution_failure"),
                    Value::Null,
                    json!([])
                ),
                (
                    Some(true),
                    json!("execution_failure"),
                    Value::Null,
                    json!([])
                ),
            ]
        );
    }

    /// Charges a fixed cost the first time a root is warmed, standing in for an engine launch and
    /// its index build. A real engine cannot be assumed present in the default suite, and an
    /// injected cost is what makes the bound below a fact rather than a race.
    struct Slow {
        opens: std::sync::atomic::AtomicUsize,
        cost: std::time::Duration,
    }

    impl crate::Warmer for Slow {
        fn open(
            &self,
            _root: PathBuf,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
        {
            Box::pin(async move {
                self.opens
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(self.cost).await;
                Ok(())
            })
        }

        fn close(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    /// The cost charged for one warm-up. Large enough that the first call cannot finish inside the
    /// noise of a shared host, small enough that the test stays quick.
    const WARM_UP: std::time::Duration = std::time::Duration::from_millis(300);

    /// What the second call must come in under. Deliberately half the warm-up rather than something
    /// tight: four agents share this host, so a bound within a few milliseconds of the truth would be
    /// a flaky test rather than a strong one. The second call does a mutex acquisition, a recorded
    /// runner reply and an in-memory citation index, so the real figure is microseconds and the
    /// margin here is three orders of magnitude.
    const SECOND_CALL_BOUND: std::time::Duration = std::time::Duration::from_millis(150);

    #[tokio::test]
    async fn the_engine_is_warmed_once_per_root_so_the_second_index_backed_call_skips_the_warm_up()
    {
        let warmer = Arc::new(Slow {
            opens: std::sync::atomic::AtomicUsize::new(0),
            cost: WARM_UP,
        });
        let runner = Recorded::replying(0, "# Trace: save\n\nindex: complete\n", "");
        let server = KtsenseServer::warming(
            runner.clone(),
            Arc::new(crate::WarmEngines::new(warmer.clone())),
        );
        let trace = || {
            Parameters(TraceParams {
                symbol: "save".to_string(),
                pick: None,
                depth: None,
                limit: None,
                root: None,
            })
        };

        let started = std::time::Instant::now();
        let first_result = server.trace_kotlin_symbol(trace()).await.expect("first");
        let first = started.elapsed();
        let started = std::time::Instant::now();
        let second_result = server.trace_kotlin_symbol(trace()).await.expect("second");
        let second = started.elapsed();

        let commands: Vec<String> = runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|request| format!("{} {:?}", request.args[0], request.format))
            .collect();
        assert_eq!(
            (
                warmer.opens.load(std::sync::atomic::Ordering::Relaxed),
                commands,
                first >= WARM_UP,
                second < SECOND_CALL_BOUND,
                warmth(&first_result),
                warmth(&second_result),
            ),
            (
                1,
                vec![
                    "status Json".to_string(),
                    "trace Md".to_string(),
                    "trace Md".to_string(),
                ],
                true,
                true,
                Some(json!("opened")),
                Some(json!("already_decided")),
            ),
            "first={first:?} second={second:?}"
        );
    }
}
