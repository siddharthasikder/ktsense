# Changelog

## Unreleased

### Added

- `map --compact` (and the `compact` argument of the `get_kotlin_repo_map` MCP tool) lists each mapped file's public declarations as kind and name only, with no signatures, supertypes or KDoc, so the same token budget covers far more of a module. On `ktor-server/ktor-server-core` at `--budget 4000` it surfaces 130 of the module's 136 public top-level types, against 40 for the default map. The default map is byte-identical to before. The routed daemon request shape gained the flag, so the daemon protocol version is now 6 (KT-97).
- `context --only <section>[,<section>...]` (and the `only` argument of the `explain_kotlin_symbol` MCP tool) renders the declaration line plus only the named sections, out of `source`, `callers`, `implementors` and `outline`, and spends the token budget on them alone. The full bundle is the default and is byte-identical to before, in Markdown and JSON; a filtered bundle carries a `sections` object in its JSON. The routed daemon request shape gained the filter, so the daemon protocol version is now 4 (KT-96).
- `context --match <regex> [--around N]` (and the `match` and `around` arguments of the `explain_kotlin_symbol` MCP tool) keeps the declaration line and, in the Source section, only the lines matching the pattern plus `--around N` context lines (default 1), each with its line number and `...` where lines were skipped. It composes with `--only`. Without `--match` the output is unchanged. The routed daemon request gained the pattern, so the daemon protocol version is now 5 (KT-101).

### Changed

- `--pick` on `symbols`, `trace` and `context`, and the `pick` argument of the `find_kotlin_symbol`, `trace_kotlin_symbol` and `explain_kotlin_symbol` MCP tools, now accepts a dot-boundary suffix of a fully-qualified name as well as a full FQN, such as `InMemoryOrderRepository.save` for `shop.db.InMemoryOrderRepository.save`. An exact FQN wins; a suffix matching one candidate selects it; a suffix matching several lists only those and exits 3; a suffix matching none keeps the pick-missed error. The ambiguity hint now names the shortest unique dot-boundary suffix of its example candidate rather than the whole FQN, so a rerun copies less (KT-95).

## 0.1.0

First stable release. It follows the `v0.0.1-rc.2` prerelease.

### Changed

- `context` (and the `explain_kotlin_symbol` MCP tool) now routes through the warm daemon, answering on the daemon's own session as a depth-1 `trace` instead of opening a fresh engine per call (KT-89).

### Fixed

- `symbols` now qualifies a class, object or function declared inside a function or property body. Such a local declaration was reported with no kind, package or enclosing chain because the file skeleton drops it with the body; the enrichment now falls back to a tree-sitter walk that finds it, so it carries its real kind, its package and a qualified name through its enclosing declarations (for example `io.ktor.server.application.ApplicationPluginTest.test_routing_scoped_install.Config`), is marked `(local)` in Markdown and `"local": true` in JSON, and is selectable with `--pick`. Top-level and member declarations are unchanged (KT-98).
- Signatures, supertypes and annotations lifted across several source lines now render on one line. The syntax adapter folds each such span through a pure `ktsense-core::text` normalizer that collapses whitespace runs, drops spaces just inside brackets and before commas, and drops a trailing comma before a closer, while leaving string literals and comments byte for byte so a default like `= "a  b"` and a multi-line raw string keep their contents (KT-92).

- `trace` and `context` no longer count comment, KDoc or string text, or a same-named declaration such as a `companion object`, as a caller. Each reference site is classified by the syntax node it falls in, only code uses become callers and usage rows, and the Usages line says how many text mentions and same-named declarations it left out (KT-83).
- A "no declaration named X" answer now says it searched this workspace only, not library dependencies (KT-87).
- `trace` and `context` no longer list a class as its own implementor (KT-77).
- A class that Kotlin makes final (not `open`, `abstract`, `sealed`, `enum` or `expect`) now reports no implementors. Before, same-named classes such as `FooTest` could be listed, because kmp-lsp 0.26.0 answers implementation requests on a final class with name matches. A partially parsed file keeps the engine's answer (KT-78).
- `symbols` candidates, including the ambiguous-name listing shared by `trace` and `context`, are printed in a stable order: qualified name, path, line (KT-81).
- The `trace` Usages heading now accounts for every site. When sites are left out by the per-file limit or as imports, the section says how many and why (KT-82).
- Concurrent `daemon start` calls decide who spawns with one atomic claim, so two starts can no longer both report success (KT-71).
- The MCP server instruction string names both version-guarded tools (KT-70).

### Added

- `trace` and `context` no longer end a name the workspace does not declare with a bare exit 1. After confirming no declaration resolves, the answer lists where the name appears as text in the workspace's Kotlin sources under `## Text references (N sites in M files)`, grouped by file, capped by `--limit`, and marked `precision: text match` so it is never read as resolved usages; comment and string mentions are listed but counted apart from code. The KT-87 scope wording stays and the exit stays 1, but the listing moves onto stdout so a routed daemon and the MCP tools carry the same answer. JSON carries the sites. This is what turns `putMetric`, `@RequestRouter` and generated `*RouterWrapper` names from nothing-to-act-on into the use sites a text search would have found (KT-94).

- `ktsense symbols --contains <query>` lists every declaration whose simple name contains the query, drawn from the workspace's own syntax index rather than the engine, so it needs no warm index and states `source: syntax index`. Matches are ranked exact name first, then prefix, then shorter name, then qualified name, path and line, capped by `--limit` (default 50) with a note when more were found; a query nothing contains exits with the workspace-scope wording. The `find_kotlin_symbol` MCP tool gains a `contains` flag (KT-88).

- `ktsense map` now says where its omitted files live. When files are dropped for budget, the Markdown map ends with an `Omitted: <dir> (<count>), ...` line grouping them by directory, most files first, truncated to `and N more directories` when the reserved room runs out; the summary's cost is reserved before files are chosen so the total stays within the budget. The map also states how many read files declare nothing public and were not mapped, and the JSON gains `omitted_directories` and `files_without_public_declarations` (KT-93).

- `trace` lists production callers under `## Callers` and test callers under a following `## Test callers`, at every `--depth` level, and `context` orders production callers ahead of test ones and labels the test ones, so who calls a symbol in production reads before who exercises it in tests. A caller counts as a test by its source path (a `test`, `androidTest`, `testFixtures`, `commonTest`, `jvmTest` or `<flavour>Test` source set, or a `Test.kt` / `Tests.kt` / `Spec.kt` file); JSON gains a `test` flag per caller (KT-91).
- `outline --annotations` shows each declaration's annotations, one per line above it; the default outline drops them and says how many it hid, and the `get_kotlin_outline` MCP tool gains an `annotations` argument (KT-90).
- `context` includes a `## Source` section with the declaration's own body, right after its signature. A body too long for the budget is cut on a line boundary with the omitted range to read (KT-86).
- On an ambiguous name, `trace` and `context` title the block `## Ambiguous: <name> (N candidates)` and print a `rerun with --pick <FQN>` hint on stdout and stderr, still exiting 3 (KT-85).
- `status` reports whether `rg` is on `PATH` and warns when it is missing, because engine `find` and references return nothing without it (KT-80).
- `outline` states how many private or internal declarations it hid and that `--private` includes them (KT-79).
- `scripts/install.sh`, a one-script installer. It asks before trusting the tap, installs ripgrep, and proves the install by running the binary (KT-75).
- README routing guidance on when ktsense's structured context helps and when literal grep is the better tool. The README makes no speed claim.

### Documentation

- The bundled engine's glibc floor (2.28) and the `KTSENSE_LSP_PATH` workaround are stated across all shipped docs (KT-72, KT-73).
- The Homebrew 7 tap trust step is documented (KT-74).

### Known limitations

- Resolution is syntactic, not type-checked. For an interface or open class, `Implementors` can still include a name-matched declaration from another package; supertype resolution is planned as KT-84.
- On Linux hosts below glibc 2.28 the bundled engine does not start. Set `KTSENSE_LSP_PATH` to a host-built `kmp-lsp` 0.26.0.
- In a like-for-like benchmark on ktor (KT-76), grep was faster on every question. ktsense's advantage is structured, smaller answers for questions such as callers of a nested type, not speed.
