//! The rmcp server: eight tools, each an invocation of the `ktsense` binary through a [`Runner`].

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

/// Exit codes the CLI documents as answers rather than failures: success, and an ambiguous name,
/// whose output is the candidate list an agent picks from.
const ANSWER_EXITS: [i32; 2] = [0, 3];

/// What one command invocation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
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

/// One command to run: the arguments after the global flags, and the root to answer about when the
/// call names one. The runner owns the default root, so a call's root replaces it rather than
/// being added alongside, which the CLI would refuse as a repeated flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub args: Vec<String>,
    pub root: Option<String>,
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

/// Runs the `ktsense` binary at `binary` with the configured default root and Markdown output.
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
                .arg("md")
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

    async fn invoke(&self, request: Request) -> Result<CallToolResult, ErrorData> {
        let invocation = self
            .runner
            .run(request)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(present(invocation))
    }
}

/// An exit the CLI documents as an answer becomes the answer text; anything else becomes a tool
/// error carrying the command's own message, so the agent sees `no declaration named X` rather
/// than a bare failure.
fn present(invocation: Invocation) -> CallToolResult {
    match invocation.code {
        Some(code) if ANSWER_EXITS.contains(&code) => {
            CallToolResult::success(vec![ContentBlock::text(invocation.stdout)])
        }
        _ => {
            let message = if invocation.stderr.trim().is_empty() {
                invocation.stdout
            } else {
                invocation.stderr
            };
            CallToolResult::error(vec![ContentBlock::text(message.trim().to_string())])
        }
    }
}

/// Builds the `<command> <args...>` part of a request.
struct Args(Request);

impl Args {
    fn command(command: &str, root: Option<String>) -> Self {
        Self(Request {
            args: vec![command.to_string()],
            root,
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
        description = "Compressed declaration skeleton of one Kotlin file: signatures without bodies. cost: fast (no index). Prefer it over reading the file when you need the API surface, not the implementation."
    )]
    async fn get_kotlin_outline(
        &self,
        Parameters(params): Parameters<OutlineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("outline", params.root)
            .positional(params.file)
            .flag("private", params.private)
            .flag("kdoc", params.kdoc);
        self.invoke(args.0).await
    }

    #[tool(
        name = "find_kotlin_symbol",
        description = "Find declarations by name across the workspace, with kind, file, line and signature. cost: needs_index. Several matches come back as a list to pick from; pass pick with the fully-qualified name to narrow."
    )]
    async fn find_kotlin_symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("symbols", params.root)
            .positional(params.query)
            .option("kind", params.kind)
            .option("limit", params.limit)
            .option("pick", params.pick);
        self.invoke(args.0).await
    }

    #[tool(
        name = "trace_kotlin_symbol",
        description = "Definition, implementors, callers and every reference site of one symbol, with an index: complete or partial marker. cost: needs_index. Prefer it over grep for who-calls and who-implements questions."
    )]
    async fn trace_kotlin_symbol(
        &self,
        Parameters(params): Parameters<TraceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("trace", params.root)
            .positional(params.symbol)
            .option("pick", params.pick)
            .option("depth", params.depth)
            .option("limit", params.limit);
        self.invoke(args.0).await
    }

    #[tool(
        name = "analyze_kotlin_dependencies",
        description = "Import graph of the workspace at package or file level, with cycles and external imports. cost: fast (no index). Edges are import statements, not type-checked references."
    )]
    async fn analyze_kotlin_dependencies(
        &self,
        Parameters(params): Parameters<DepsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("deps", params.root).option("level", params.level);
        self.invoke(args.0).await
    }

    #[tool(
        name = "get_kotlin_repo_map",
        description = "Token-budgeted map of the most central files and their signatures, most central first. cost: fast (no index). Start here on an unfamiliar repository."
    )]
    async fn get_kotlin_repo_map(
        &self,
        Parameters(params): Parameters<MapParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("map", params.root).option("budget", params.budget);
        self.invoke(args.0).await
    }

    #[tool(
        name = "check_kotlin_syntax",
        description = "Syntax-check a Kotlin file or directory and report each error with its position. cost: fast (no index). Run it after every edit; it is syntax only, type errors are Gradle's job."
    )]
    async fn check_kotlin_syntax(
        &self,
        Parameters(params): Parameters<CheckParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("check", params.root).positional(params.path);
        self.invoke(args.0).await
    }

    #[tool(
        name = "explain_kotlin_symbol",
        description = "Budgeted context bundle for one symbol: its declaration, the files it depends on and the callers that matter. cost: needs_index."
    )]
    async fn explain_kotlin_symbol(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("context", params.root)
            .positional(params.symbol)
            .option("budget", params.budget);
        self.invoke(args.0).await
    }

    #[tool(
        name = "ktsense_status",
        description = "Index phase, counts, uptime and engine version. cost: fast. Call it before a precision query when you need to know whether the index is complete."
    )]
    async fn ktsense_status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Args::command("status", params.root);
        self.invoke(args.0).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KtsenseServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ktsense", env!("CARGO_PKG_VERSION")))
            .with_instructions(
            "Kotlin code understanding for agents. Tools marked cost: fast parse with tree-sitter \
             and need no index; tools marked cost: needs_index start or reuse an engine session \
             and their answers carry an index marker. Resolution is syntactic, not type-checked.",
        )
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
    use crate::{Cost, TOOLS};

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

    fn catalogued_cost(name: &str) -> Option<Cost> {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.cost)
    }

    fn is_object_schema(schema: &Value) -> bool {
        schema.get("type") == Some(&json!("object")) && schema.get("properties").is_some()
    }

    #[test]
    fn lists_the_eight_catalogued_tools_each_with_an_object_schema_and_its_cost_hint() {
        let server = KtsenseServer::new(Recorded::replying(0, "", ""));
        let listed = server.listed_tools();

        let mut names: Vec<String> = listed.iter().map(|tool| tool.name.to_string()).collect();
        names.sort();
        let mut catalogued: Vec<String> = TOOLS.iter().map(|tool| tool.name.to_string()).collect();
        catalogued.sort();
        let schemas_valid = listed
            .iter()
            .all(|tool| is_object_schema(&Value::Object((*tool.input_schema).clone())));
        let costs_stated = listed.iter().all(|tool| {
            let description = tool.description.as_deref().unwrap_or_default();
            let cost = catalogued_cost(&tool.name).expect("catalogued");
            description.contains(&format!("cost: {}", cost.label()))
        });

        assert_eq!(
            (listed.len(), names, schemas_valid, costs_stated),
            (8, catalogued, true, true)
        );
    }

    #[tokio::test]
    async fn a_tool_call_builds_the_request_with_its_root_override_and_returns_stdout_as_the_answer(
    ) {
        let runner = Recorded::replying(0, "# Trace: shop.order.OrderRepository.save\n", "");
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
                }],
                Some(false),
                json!("# Trace: shop.order.OrderRepository.save\n"),
            )
        );
    }

    #[tokio::test]
    async fn an_ambiguous_exit_is_an_answer_and_a_failure_exit_is_a_tool_error_with_stderr() {
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
        assert_eq!(
            (
                listed.is_error,
                text(&listed),
                missing.is_error,
                text(&missing)
            ),
            (
                Some(false),
                json!("## Symbols: save\n"),
                Some(true),
                json!("ktsense: no declaration named ZzzNope"),
            )
        );
    }
}
