# Wiring ktsense into an agent

`ktsense mcp` is an MCP server that speaks JSON-RPC over stdio and exposes eight tools for
understanding a Kotlin workspace. Any MCP client that can launch a local command can use it. This
page covers Kiro CLI, Claude Code and Codex, and shows how to verify the server before you involve a
client at all.

The tools are `get_kotlin_outline`, `find_kotlin_symbol`, `trace_kotlin_symbol`,
`analyze_kotlin_dependencies`, `get_kotlin_repo_map`, `check_kotlin_syntax`, `explain_kotlin_symbol`
and `ktsense_status`. `contrib/agent-skill/SKILL.md` is a skill file that teaches an agent when to
reach for which, and is worth installing alongside the server.

## Before you configure a client

You need the `ktsense` binary, and for most of the tools the `kmp-lsp` engine it drives.

Build the binary with `cargo build --release`, which leaves it at `target/release/ktsense`, and
install the engine yourself for now. ktsense looks for it in three places, in order: the path in
`KTSENSE_LSP_PATH`, then `libexec/kmp-lsp` beside the binary's own directory, then `kmp-lsp` on
`PATH`. That middle location is where a packaged release carries its own copy, and the published
`v0.0.1-rc.2` prerelease tarballs do carry one: each holds `bin/ktsense` beside `libexec/kmp-lsp`, so
an unpacked tarball finds its engine with nothing configured. A source build brings no engine with it,
so for that one it is the first and third that matter. An override pointing at a file that does not
exist does not fail: discovery skips the missing candidate and falls through to the bare name, so a
typo in `KTSENSE_LSP_PATH` silently gets you whichever `kmp-lsp` is on `PATH` instead of an error.

Three tools need no engine at all, because they are pure tree-sitter: `get_kotlin_outline`,
`analyze_kotlin_dependencies` and `get_kotlin_repo_map`. Every other tool reaches the engine, and
that includes `check_kotlin_syntax`, which is a passthrough to the engine's own checker even though it
waits for no index. Each tool description carries a three-state `requires:` marker that says which of
those it is, `nothing`, `kmp-lsp`, or `kmp-lsp and a settled index`, and a separate `cost:` marker for
what the call takes. That replaced KT-31's single `fast | needs_index` marker, which said only whether
a tool waited for the reference index and was read as saying it needed no engine binary (KT-32). On a
host without `kmp-lsp` a syntax check fails just as a trace does.

The version ktsense is built against is pinned in `ktsense-lsp::PINNED_UPSTREAM_VERSION`, and it is
probed only where a tool opens an engine session. Today that means `trace_kotlin_symbol`,
`explain_kotlin_symbol` and the warm daemon: the located binary is run with `--version` and classified
against the pin, so a matching major and minor starts silently, a different minor starts with a
warning, and a different major or an unreadable version is refused before any session child is
spawned. `explain_kotlin_symbol` is guarded because the bundle it builds is a depth-1 trace driven
through the same fresh-session path, not because it probes on its own. The one-shot passthroughs behind
`check_kotlin_syntax` and `find_kotlin_symbol` do not probe at all, and neither does the symbol
resolution `trace_kotlin_symbol` performs before it opens its session. A wrong-major engine is
therefore refused for a trace and used without complaint for a syntax check, so do not read the pin as
a guarantee that an incompatible engine cannot be used.

Use absolute paths everywhere in a client configuration. The client launches the server as a child
process, and a relative path resolves against whatever working directory the client happened to have.

## Choosing the root

Pass `--root` when you launch the server, and point it at the repository you want answers about:

```
ktsense --root /home/you/src/my-kotlin-app mcp
```

This matters more than it looks. The server canonicalizes that root once and passes it to every
delegated command, which is what stops the underlying engine from falling back to its own default of
the nearest enclosing `.git` directory. A root that is wider than you intended does not fail; it
answers about the wrong code. If you omit `--root` entirely, the server takes the client's working
directory, which is rarely what you want.

One server per repository is the simplest arrangement. If you would rather run one server and
redirect it, every tool accepts an optional `root` argument that replaces the launch root for that
one call.

## Kiro CLI

Add the server to `~/.kiro/settings/mcp.json`, creating the file if it does not exist:

```json
{
  "mcpServers": {
    "ktsense": {
      "command": "/home/you/.local/bin/ktsense",
      "args": ["--root", "/home/you/src/my-kotlin-app", "mcp"],
      "timeout": 120000
    }
  }
}
```

The same shape works in a workspace-scoped `.kiro/settings/mcp.json`, which takes precedence over the
global file and keeps the server scoped to one checkout. Either file can be written for you:

```
kiro-cli mcp add --name ktsense --scope workspace \
  --command /home/you/.local/bin/ktsense \
  --args --root --args /home/you/src/my-kotlin-app --args mcp
```

There is a wrinkle worth knowing before you debug the wrong thing. A server declared in a
`settings/mcp.json` file reaches a chat session only if the active agent opts into those files with
`"useLegacyMcpJson": true`; an agent configuration without it sees no ktsense tools however correct
the JSON is. On the host this page was written against, a custom agent with `"tools": ["*"]` and no
opt-in listed none of the tools, and the same agent with the opt-in listed all eight. If you would
rather not depend on that, declare the server in the agent configuration itself, where it is always
honoured:

```json
{
  "name": "kotlin-dev",
  "tools": ["@ktsense"],
  "allowedTools": ["@ktsense"],
  "mcpServers": {
    "ktsense": {
      "command": "/home/you/.local/bin/ktsense",
      "args": ["--root", "/home/you/src/my-kotlin-app", "mcp"],
      "timeout": 120000
    }
  }
}
```

Save that as `.kiro/agents/kotlin-dev.json` for one workspace or `~/.kiro/agents/kotlin-dev.json`
globally, and start a session with `kiro-cli chat --agent kotlin-dev`. Either way, `/mcp` in a running
session shows whether the server initialized, and `/tools` lists the tools it contributed.

## Claude Code

`claude mcp add` takes the name, then the command and its arguments after a `--` separator:

```
claude mcp add ktsense -- /home/you/.local/bin/ktsense \
  --root /home/you/src/my-kotlin-app mcp
```

The `--` matters: without it the flags are parsed as flags to `claude`. Transport defaults to stdio,
which is what ktsense speaks, so there is nothing to select. `--scope` decides how far the entry
reaches: `local` (the default) for the current project only, `project` to share it through the
repository, `user` to have it everywhere. `claude mcp list` shows what is registered and
`claude mcp remove ktsense` undoes it.

## Codex

Add a table to `~/.codex/config.toml`:

```toml
[mcp_servers.ktsense]
command = "/home/you/.local/bin/ktsense"
args = ["--root", "/home/you/src/my-kotlin-app", "mcp"]
```

The table name after the dot is the server name. The CLI will write the same entry for you:

```
codex mcp add ktsense -- /home/you/.local/bin/ktsense \
  --root /home/you/src/my-kotlin-app mcp
```

As with Claude Code the `--` separates the launch command from Codex's own flags. `codex mcp list`
and `codex mcp get ktsense` read the configuration back, and `codex mcp remove ktsense` deletes it.

## Verifying without a client

A client that reports nothing does not tell you which half is at fault, so it is worth proving the
server directly. `ktsense mcp` reads newline-delimited JSON-RPC on stdin, so three lines are enough
to get the tool list:

```
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | ktsense --root /home/you/src/my-kotlin-app mcp
```

The first response identifies the server as `ktsense` and advertises a `tools` capability; the second
carries all eight tools with their input schemas. The server exits 0 when stdin closes, which is how
a client shuts it down, so the pipe ending is a clean exit rather than a crash.

If that works and your client still shows nothing, the problem is in the client configuration: a
relative command path, a typo in the JSON or TOML, or on Kiro the agent question described above.

## An optional warm session

`ktsense daemon start --root <dir>` keeps an engine session warm for a root, and `get_kotlin_outline`,
`analyze_kotlin_dependencies`, `get_kotlin_repo_map` and `trace_kotlin_symbol` route through it
automatically when it is live, falling back to running in process when it is not. It expires after an
hour idle, and `ktsense daemon status` and `ktsense daemon stop` inspect and end it.

Nothing requires it. Three of those four are the tree-sitter tools that were already fast, so the
daemon is a convenience rather than a prerequisite. `trace_kotlin_symbol` is the one that gains from
it: a routed trace answers on the daemon's own warm session, and on a complete index resolves its
symbol from that session rather than paying for a fresh one. The rest, `find_kotlin_symbol` and
`explain_kotlin_symbol` included, still do their own work per call.

## Troubleshooting

A tool error reading `is not implemented yet` would be a command that has not shipped rather than a
misconfiguration, and no tool answers that today. All eight are implemented: `ktsense_status` since
KT-36 and `explain_kotlin_symbol` since KT-35, whose `context` command was the last one the surface
advertised without implementing. Exit code 70 has had no caller since, and stays in the contract for
the next surface-first command.

An answer marked `index: partial` means the reference index had not finished when the question was
answered. It is a real answer about what was indexed, not a complete one. Ask again once the index has
had time, or pass `--wait-index` when driving `trace` from the CLI.

An error from `check_kotlin_syntax` or `find_kotlin_symbol` where the tool list itself worked is a
missing or unusable engine rather than a broken client: the client found `ktsense` and `ktsense` could
not usefully run `kmp-lsp`. Check `KTSENSE_LSP_PATH` and `PATH` before touching the client
configuration, and bear in mind that a bad override falls through to `PATH` rather than failing, so
the engine you get may not be the one you named. Neither of those two tools probes the engine version,
so an incompatible engine shows up as a confusing answer rather than a refusal.

A `command not found` from the client almost always means a relative or wrong `command` path. Run the
same command by hand from a different directory to confirm.

For engine problems, `KTSENSE_LOG=debug` raises the log level on stderr. Note that the server's stdout
is the protocol channel, so diagnostics belong on stderr and a client that mixes the two will see
corrupted JSON.

## Sources

The client behaviour described on this page was checked on the host it was written against, on
2026-09-21, rather than taken from memory.

- `kiro-cli mcp --help`, `kiro-cli mcp add --scope workspace`, and Kiro's bundled MCP and agent
  configuration documentation, for the config file locations, the JSON schema and the
  `useLegacyMcpJson` behaviour. Verified by listing the tools in a throwaway workspace.
- `claude mcp add --help`, for the argument order, the `--` separator and the scope values.
- `codex mcp --help` and `codex mcp add --help`, plus the `[mcp_servers.<name>]` shape already present
  in `~/.codex/config.toml`.
- ⚠️ External link - [Kiro CLI MCP documentation](https://kiro.dev/docs/cli/mcp/security/) - accessed
  2026-09-21, referenced from the bundled documentation for administrator-side MCP governance.
