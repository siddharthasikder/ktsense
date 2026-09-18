//! MCP stdio server exposing the ktsense commands as agent tools.
//!
//! The eight tools are the [`TOOLS`] catalogue below, kept as data because the CLI, the docs and
//! the agent skill file all have to agree with it. Each tool delegates to the `ktsense` binary
//! itself, run as a child process with `--format md`: the commands, their exit codes and their
//! neutralised output already carry the product's honesty rules, and this crate must not depend on
//! the binary crate, so the process boundary is the seam. The [`Runner`] port makes that seam
//! explicit, and lets the tool layer be tested with a recorded runner and no subprocess.
//!
//! Exit codes map onto MCP results like this: `0` and `3` are answers (an ambiguous name is a real
//! answer, the candidate list, and the text says to pick one); every other status is a tool error
//! carrying whatever the command wrote to stderr.

#![forbid(unsafe_code)]

mod server;

pub use server::{
    serve, ExecutableRunner, Invocation, KtsenseServer, Request, Runner, RunnerError, ServerConfig,
};

/// Whether a tool needs the engine index, which tells an agent what it will cost to call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    /// Answered from tree-sitter parsing alone: no index, no engine session.
    Fast,
    /// Requires the engine index, so the first call on a cold repo may wait.
    NeedsIndex,
}

impl Cost {
    pub fn label(self) -> &'static str {
        match self {
            Cost::Fast => "fast",
            Cost::NeedsIndex => "needs_index",
        }
    }
}

/// One agent-facing tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tool {
    pub name: &'static str,
    pub cli_command: &'static str,
    pub cost: Cost,
}

/// Every tool the MCP server exposes, paired with the CLI command it delegates to.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "get_kotlin_outline",
        cli_command: "outline",
        cost: Cost::Fast,
    },
    Tool {
        name: "find_kotlin_symbol",
        cli_command: "symbols",
        cost: Cost::NeedsIndex,
    },
    Tool {
        name: "trace_kotlin_symbol",
        cli_command: "trace",
        cost: Cost::NeedsIndex,
    },
    Tool {
        name: "analyze_kotlin_dependencies",
        cli_command: "deps",
        cost: Cost::Fast,
    },
    Tool {
        name: "get_kotlin_repo_map",
        cli_command: "map",
        cost: Cost::Fast,
    },
    Tool {
        name: "check_kotlin_syntax",
        cli_command: "check",
        cost: Cost::Fast,
    },
    Tool {
        name: "explain_kotlin_symbol",
        cli_command: "context",
        cost: Cost::NeedsIndex,
    },
    Tool {
        name: "ktsense_status",
        cli_command: "status",
        cost: Cost::Fast,
    },
];

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
}
