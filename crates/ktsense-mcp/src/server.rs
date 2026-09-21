//! The rmcp server: eight tools, each an invocation of the `ktsense` binary through a [`Runner`].

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, JsonObject, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::citations::{index_answer, Answer};
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
}

/// How the server finds the binary it delegates to and which workspace it answers about by
/// default. The CLI passes its own executable path.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub binary: PathBuf,
    pub root: PathBuf,
}

/// Serves the tools over stdio until the client disconnects.
pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    let runner = ExecutableRunner::new(config.binary, config.root);
    let running = KtsenseServer::new(Arc::new(runner))
        .serve(rmcp::transport::stdio())
        .await?;
    running.waiting().await?;
    Ok(())
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

/// The MCP server. Every tool builds a CLI argument list and hands it to the runner.
#[derive(Clone)]
pub struct KtsenseServer {
    runner: Arc<dyn Runner>,
    tool_router: ToolRouter<Self>,
}

impl KtsenseServer {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            tool_router: Self::tool_router(),
        }
    }

    /// The tools as the client will list them, for tests that check the catalogue is honoured.
    pub fn listed_tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    async fn invoke(&self, tool: &Tool, request: Request) -> Result<CallToolResult, ErrorData> {
        let invocation = self
            .runner
            .run(request)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(present(tool, invocation))
    }
}

/// An exit the CLI documents as an answer becomes the answer text; anything else becomes a tool
/// error carrying the command's own message, so the agent sees `no declaration named X` rather
/// than a bare failure. Either way the result carries its citation index, because the files a
/// failure names are as worth following as the ones an answer names.
fn present(tool: &Tool, invocation: Invocation) -> CallToolResult {
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
    let indexed = index_answer(tool, &invocation.root, invocation.code, &text);
    let content = vec![ContentBlock::text(text)];
    let mut result = if answered {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    result.structured_content = serde_json::to_value(&indexed).ok();
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
        let args = Args::for_tool(&crate::OUTLINE, params.root)
            .positional(params.file)
            .flag("private", params.private)
            .flag("kdoc", params.kdoc);
        self.invoke(&crate::OUTLINE, args.0).await
    }

    #[tool(
        name = "find_kotlin_symbol",
        description = "Answers where a name is declared: kind, file, line and signature for every declaration carrying it. Prefer it over grep, which also finds uses and cannot tell a class from a local. Several matches come back as a list to choose from; pass pick with one fully-qualified name to narrow. requires: kmp-lsp. cost: about 300 ms on 1861 files (ktor 3.0.1, median of 9, KT-38).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn find_kotlin_symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::for_tool(&crate::SYMBOLS, params.root)
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
        let args = Args::for_tool(&crate::TRACE, params.root)
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
        let args = Args::for_tool(&crate::DEPS, params.root).option("level", params.level);
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
        let args = Args::for_tool(&crate::MAP, params.root).option("budget", params.budget);
        self.invoke(&crate::MAP, args.0).await
    }

    #[tool(
        name = "check_kotlin_syntax",
        description = "Answers whether a file or a directory parses, reporting every syntax error with its position. Run it after each edit. It is syntax only: type errors are the compiler's job and are not reported, and a file with errors comes back as an error result carrying the report. requires: kmp-lsp, which has to be installed even though no index is waited for. cost: about 145 ms for one 54 KB file (ktor 3.0.1, median of 9, KT-32).",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn check_kotlin_syntax(
        &self,
        Parameters(params): Parameters<CheckParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::for_tool(&crate::CHECK, params.root).positional(params.path);
        self.invoke(&crate::CHECK, args.0).await
    }

    #[tool(
        name = "explain_kotlin_symbol",
        description = "Will answer with a budgeted bundle for one symbol: its declaration, the files it depends on and the callers that matter. Not implemented yet (KT-35), so every call returns ktsense's not-implemented message as an error; reach for find_kotlin_symbol and trace_kotlin_symbol instead. requires: kmp-lsp and a settled index. cost: unmeasured.",
        annotations(read_only_hint = true, open_world_hint = false),
        output_schema = answer_schema()
    )]
    async fn explain_kotlin_symbol(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::for_tool(&crate::CONTEXT, params.root)
            .positional(params.symbol)
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
        let args = Args::for_tool(&crate::STATUS, params.root);
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
    }

    fn catalogued(name: &str) -> &'static Tool {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("listed tools come from the catalogue")
    }

    fn is_object_schema(schema: &Value) -> bool {
        schema.get("type") == Some(&json!("object")) && schema.get("properties").is_some()
    }

    #[test]
    fn every_listed_tool_is_catalogued_read_only_and_states_its_requirement_and_pending_card() {
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
                && entry
                    .pending_card
                    .is_none_or(|card| description.contains(card))
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
}
