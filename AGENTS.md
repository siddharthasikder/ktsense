# AGENTS.md - ktsense

ktsense is a Rust CLI and MCP server that gives terminal AI agents compressed, structured answers
about a Kotlin codebase. It wraps the upstream `kmp-lsp` engine by process; it does not link it.

## Layering (enforced by review, and by dependency direction in Cargo.toml)

| Crate | Role | May depend on |
|---|---|---|
| `ktsense-core` | Pure domain: skeleton model, compressors, ranking, budgeting | `serde` only |
| `ktsense-syntax` | tree-sitter Kotlin to core values | core, tree-sitter |
| `ktsense-lsp` | Client for the `kmp-lsp` child process | core, lsp-types, tokio |
| `ktsense-daemon` | Warm session over a Unix socket | core, lsp |
| `ktsense-mcp` | MCP stdio server | core, syntax, lsp |
| `ktsense-cli` | `ktsense` binary | everything above |

`ktsense-core` must never gain a dependency on the filesystem, a process, a runtime, or a parser. If
an algorithm needs one, the algorithm is in the wrong crate. This is what keeps ranking, budgeting
and compression testable from hand-built values.

## Upstream engine

- Pinned version lives in `ktsense-lsp::PINNED_UPSTREAM_VERSION`. Bump it deliberately, per release.
- Binary lookup order: `KTSENSE_LSP_PATH`, then `<exe>/../libexec/kmp-lsp`, then `PATH`.
- Spawn children with `RUST_LOG=error`: upstream writes `env_logger` INFO lines into the same stream
  as command output, which corrupts parsing otherwise.
- Send the LSP `initialized` notification immediately after the `initialize` response. Without it,
  `shutdown` and `exit` do not terminate the child and it has to be killed.
- `kmp-lsp` 0.26.0 has **no** `callHierarchyProvider`. Callers come from `references` plus the
  enclosing declaration of each reference site, never from call hierarchy.

## Accuracy honesty

Resolution is syntactic (tree-sitter), not type-checked. Never present a result as certain when it
is not: ambiguous symbol lookups list every candidate, and output carries the precision level. Type
errors are Gradle's job, and the README says so.

## Tests

- Unit tests in `ktsense-core` for every algorithm, no I/O.
- Golden snapshots (`insta`) for anything that renders text a human or an agent reads.
- Adapter tests drive a fake `kmp-lsp` replay binary, so the default `cargo test` needs no upstream
  install. Tests against the real engine are behind the `real-lsp` feature.
- Use as few assertions as possible: compose observed state into one value and assert it once.
- Never weaken an assertion to make a build pass.

## Comments

Express intent through naming and structure. Write a comment only for a non-obvious external
constraint, such as the upstream behaviours listed above. Never delete an existing comment of that
kind.

## Board

Work is tracked in `.agents/tasks/kotlin-code-understanding/kanban.md` in the parent workspace, and
mirrored at https://bunsho.amazon.dev/sfa9hcREMA4d8. Card ids (`KT-nn`) appear in commit messages and
in the not-yet-implemented messages the CLI prints.
