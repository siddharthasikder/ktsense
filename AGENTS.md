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
- **`exit` never terminates `kmp-lsp` 0.26.0; only stdin EOF does.** With or without `initialized`,
  the child is still alive seconds after `shutdown` and `exit`, while closing its stdin ends it in
  about 10 ms with exit code 0 (a bare EOF with nothing sent does the same). `LspClient::shutdown`
  therefore closes stdin after `exit` and only then waits out the grace period; anything that drives
  the engine by hand must do likewise or it will kill the child. Still send `initialized` right after
  the `initialize` response: the protocol requires it and the fake insists on it. Corrected
  2026-09-18 (KT-50); the earlier wording here blamed a missing `initialized` and had been measured
  against the fake, not the engine.
- `kmp-lsp` 0.26.0 has **no** `callHierarchyProvider`. Callers come from `references` plus the
  enclosing declaration of each reference site, never from call hierarchy.
- **Always pass `--root` to a command-mode invocation.** Upstream defaults the workspace root to the
  nearest `.git` directory, not the working directory, so an unrooted `find` or `refs` run inside
  `fixtures/multi-module` silently searches the whole ktsense repository: `refs save` returns 6 sites
  with the root and 9 without, the extra 3 coming from the unrelated `tiny-app` fixture. A widened
  root does not fail, it just answers about the wrong code.
- **A cold-cache `find` execs `rg`, and answers nothing at all without it.** `find` defaults to
  `--fast`, which upstream's own help describes as "use rg/fd only; never load index (default when no
  cache)". It really does exec `rg`: measured on `fixtures/multi-module` from a cold cache, `find save
  --json --root …` reports 3 declarations with `rg` on `PATH`, **0** with only `fd`, **0** with
  neither, and 3 with neither once `kmp-lsp index` has run. With no `rg` it writes empty stdout, empty
  stderr and exit 1, which is the same shape as "matched nothing", so a missing `rg` is
  indistinguishable from an absent symbol. GitHub's `ubuntu-24.04` runner ships neither tool. An
  engine session does **not** warm the cache `find` reads: the two key differently (a session wrote
  `…/kmp-lsp/8b95b6dd308fc328/index.bin` where `find --root fixtures/multi-module` looked for
  `…/b1912a32462f5c84/index.bin`), so a `find` spawned after an index is complete is still a cold
  `find`. `trace` therefore never reports absence on an empty command-mode `find` alone: it asks the
  session's own index before agreeing (KT-67, 2026-09-22).
- **`refs`, and the session's `textDocument/references`, need `rg` too.** This one has no ktsense
  workaround and degrades both paths equally, so it is not a divergence but a host requirement.
  Measured on `fixtures/multi-module` with the index complete: `refs save` reports 6 sites with `rg`
  on `PATH` and **0** without, and a `trace` answers `Callers (3)` / `Usages (6 sites in 6 files)`
  with it against `Callers (0)` / `Usages (1 site in 1 file)` without, identically through the warm
  daemon and in process. `outline` and `map` are unaffected, being tree-sitter and a directory walk.
  The `real-lsp` CI job therefore installs `ripgrep`: without it that job measures a crippled engine
  (KT-67, 2026-09-22).
- **Do not trust the column `find --json` reports.** On a cold cache that same text-search path has
  been observed to report the column of the keyword before the name (`public val CallLogging` at the
  `v`, column 8, where the indexed path says 12). A references request at that position answers with
  every use of the keyword: 14,702 sites on ktor instead of 31, under an honest `index: complete`.
  `trace` locates the name on the reported line itself and falls back to the engine's column only when
  the name is not there (KT-24, 2026-09-18).
- A `find` that matches nothing **exits 0 with empty output**. Absence cannot be read from the exit
  status; ktsense supplies its own non-zero for "no such symbol".

## Platform limits

- **A daemon socket path is an address, not a pathname.** `sockaddr_un.sun_path` holds 104 bytes on
  Darwin and 108 on Linux, so `ktsense-daemon::SOCKET_BUDGET` applies the tightest of them on every
  platform: a budget that widened on Linux would make a macOS overflow unreproducible where the work
  is done. The budget also reserves `COMPANION_RESERVE` bytes for the longest file a start puts beside
  a socket, its claim `<socket>.start`, because the KT-66 failure was a 101-byte socket that bound and
  a 108-byte claim that did not. A runtime directory too deep for that budget is answered from
  `/tmp/ktsense-<uid>/<key of the rejected directory>`; `resolve_socket_path` owns the precedence and
  is the only way to derive a socket path, since the CLI parent, the detached `daemon serve` child and
  every routed command must agree on one. Observed 2026-09-22 on macos-14 CI, where a `TempDir` under
  the per-user `TMPDIR` is already about fifty bytes deep.
- **`UnixListener::bind` is `bind(2)` and then a separate `listen(2)`, so a socket path that exists is
  not yet a socket that answers.** A connect in that gap is refused exactly as an abandoned socket is,
  and the two cannot be told apart. `daemon start`'s arbitration used to decide whether a claim's
  holder was alive by connecting to it, and read a refusal as death by moving to the next of 64 claim
  generations; a probe landing in the holder's own gap therefore minted a second claim, and both starts
  then spawned a daemon and both reported that they had started it (KT-71, macos-15 CI run
  35762259473, one race in three; 1500 races on Linux never showed it). The claim is now one file held
  under `flock`, so acquiring it is a single atomic decision and nothing observes whether the holder is
  alive. **Never reintroduce a liveness probe into that path:** whatever a start reports must come from
  the same operation that decides who spawns. The same rule retires the other way in: nothing accepts
  on a claim, so every probe consumed a backlog slot permanently, and a BSD kernel refuses a connect
  once that queue is full where Linux blocks.

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
