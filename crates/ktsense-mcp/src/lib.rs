//! MCP stdio server exposing the ktsense commands as agent tools.
//!
//! The eight tools are the [`TOOLS`] catalogue below, kept as data because the CLI, the docs and
//! the agent skill file all have to agree with it. Each tool delegates to the `ktsense` binary
//! itself, run as a child process: the commands, their exit codes and their neutralised output
//! already carry the product's honesty rules, and this crate must not depend on the binary crate,
//! so the process boundary is the seam. The [`Runner`] port makes that seam explicit, and lets the
//! tool layer be tested with a recorded runner and no subprocess.
//!
//! Exit codes map onto MCP results like this: `0` and `3` are answers (an ambiguous name is a real
//! answer, the candidate list, and the text says to pick one); every other status is a tool error
//! carrying whatever the command wrote to stderr.
//!
//! Every answer carries the command's Markdown as its text content and an [`Answer`] as its
//! `structuredContent`, so an agent can open the files a result cites without parsing prose.

#![forbid(unsafe_code)]

mod citations;
mod roots;
mod server;
mod warm;

pub use citations::{index_answer, Answer, Citation, MAX_CITATIONS};
pub use roots::ClientRoots;
pub use server::{
    serve, ExecutableRunner, Format, Invocation, KtsenseServer, Request, Runner, RunnerError,
    ServerConfig,
};
pub use warm::{EngineWarmer, WarmEngines, Warmer, Warmth, DEFAULT_WARM_UP_BOUND};

/// What has to be installed and settled before a tool can answer, which is the first thing an
/// agent needs to know about it and the one thing it cannot discover by trying.
///
/// The distinction that matters is between the engine and its index, and it is three states rather
/// than two. `check_kotlin_syntax` waits for no index, but it is an engine passthrough: on a host
/// with no `kmp-lsp` it fails on every call. A marker that only said "no index" would send an agent
/// down that path. The older `cost: fast` marker conflated both of these with being cheap, which
/// KT-38 measured as false: `analyze_kotlin_dependencies` needs neither engine nor index and still
/// takes about 1.7 seconds on 1861 files, because it parses every file in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Answered by ktsense alone, parsing with tree-sitter. No engine binary, no index.
    Nothing,
    /// Needs the `kmp-lsp` binary present, but answers without waiting for its index.
    Engine,
    /// Needs the binary and an index, and the answer carries an `index:` marker saying how far
    /// that index had got.
    EngineIndex,
}

impl Requirement {
    /// The label a tool description states, so the catalogue and the prose cannot drift.
    pub fn label(self) -> &'static str {
        match self {
            Requirement::Nothing => "nothing",
            Requirement::Engine => "kmp-lsp",
            Requirement::EngineIndex => "kmp-lsp and a settled index",
        }
    }
}

/// One agent-facing tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tool {
    pub name: &'static str,
    pub cli_command: &'static str,
    pub requires: Requirement,
    /// The card that will make this tool answer, while its backing command still returns the CLI's
    /// not-implemented error. Held as data so the description, the documentation and the session
    /// test read the same fact from one place.
    pub pending_card: Option<&'static str>,
    /// Whether a settled index changes what this tool answers, rather than only how fast.
    ///
    /// Not the same question as [`Tool::requires`], and measured rather than assumed. On ktor with an
    /// empty engine cache, `find_kotlin_symbol` comes back with four candidates for one class,
    /// because the engine's `find` falls back to a text search, and `trace_kotlin_symbol` then exits
    /// ambiguous instead of answering at all; against a settled index both are exact. So `symbols`
    /// requires no index wait and is still shaped by one. `check_kotlin_syntax` is the other side of
    /// that line: it needs the engine and parses one file, and an index would change nothing, which
    /// is why it is not made to wait for one.
    pub index_shapes_answer: bool,
}

pub const OUTLINE: Tool = Tool {
    name: "get_kotlin_outline",
    cli_command: "outline",
    requires: Requirement::Nothing,
    pending_card: None,
    index_shapes_answer: false,
};

pub const SYMBOLS: Tool = Tool {
    name: "find_kotlin_symbol",
    cli_command: "symbols",
    requires: Requirement::Engine,
    pending_card: None,
    index_shapes_answer: true,
};

pub const TRACE: Tool = Tool {
    name: "trace_kotlin_symbol",
    cli_command: "trace",
    requires: Requirement::EngineIndex,
    pending_card: None,
    index_shapes_answer: true,
};

pub const DEPS: Tool = Tool {
    name: "analyze_kotlin_dependencies",
    cli_command: "deps",
    requires: Requirement::Nothing,
    pending_card: None,
    index_shapes_answer: false,
};

pub const MAP: Tool = Tool {
    name: "get_kotlin_repo_map",
    cli_command: "map",
    requires: Requirement::Nothing,
    pending_card: None,
    index_shapes_answer: false,
};

pub const CHECK: Tool = Tool {
    name: "check_kotlin_syntax",
    cli_command: "check",
    requires: Requirement::Engine,
    pending_card: None,
    index_shapes_answer: false,
};

pub const CONTEXT: Tool = Tool {
    name: "explain_kotlin_symbol",
    cli_command: "context",
    requires: Requirement::EngineIndex,
    pending_card: Some("KT-35"),
    index_shapes_answer: true,
};

pub const STATUS: Tool = Tool {
    name: "ktsense_status",
    cli_command: "status",
    requires: Requirement::Nothing,
    pending_card: None,
    index_shapes_answer: false,
};

/// Every tool the MCP server exposes, paired with the CLI command it delegates to.
pub const TOOLS: &[Tool] = &[OUTLINE, SYMBOLS, TRACE, DEPS, MAP, CHECK, CONTEXT, STATUS];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_eight_tools_with_unique_names_and_commands() {
        let mut names: Vec<_> = TOOLS.iter().map(|tool| tool.name).collect();
        let mut commands: Vec<_> = TOOLS.iter().map(|tool| tool.cli_command).collect();
        names.sort_unstable();
        commands.sort_unstable();
        let unique_names = {
            let mut deduped = names.clone();
            deduped.dedup();
            deduped.len()
        };
        let unique_commands = {
            let mut deduped = commands.clone();
            deduped.dedup();
            deduped.len()
        };
        assert_eq!(
            (TOOLS.len(), unique_names, unique_commands),
            (8, 8, 8),
            "every tool needs a distinct name and a distinct backing command"
        );
    }

    #[test]
    fn a_requirement_names_what_must_be_installed_and_is_not_the_same_as_being_shaped_by_the_index()
    {
        let observed: Vec<(&str, &str, bool)> = TOOLS
            .iter()
            .map(|tool| {
                (
                    tool.cli_command,
                    tool.requires.label(),
                    tool.index_shapes_answer,
                )
            })
            .collect();

        assert_eq!(
            observed,
            [
                ("outline", "nothing", false),
                ("symbols", "kmp-lsp", true),
                ("trace", "kmp-lsp and a settled index", true),
                ("deps", "nothing", false),
                ("map", "nothing", false),
                ("check", "kmp-lsp", false),
                ("context", "kmp-lsp and a settled index", true),
                ("status", "nothing", false),
            ],
            "symbols waits for no index and is still shaped by one; check needs the engine and is not"
        );
    }
}
