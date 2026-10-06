# Changelog

## 0.1.0

First stable release. It follows the `v0.0.1-rc.2` prerelease.

### Changed

- `context` (and the `explain_kotlin_symbol` MCP tool) now routes through the warm daemon, answering on the daemon's own session as a depth-1 `trace` instead of opening a fresh engine per call (KT-89).

### Fixed

- A "no declaration named X" answer now says it searched this workspace only, not library dependencies (KT-87).
- `trace` and `context` no longer list a class as its own implementor (KT-77).
- A class that Kotlin makes final (not `open`, `abstract`, `sealed`, `enum` or `expect`) now reports no implementors. Before, same-named classes such as `FooTest` could be listed, because kmp-lsp 0.26.0 answers implementation requests on a final class with name matches. A partially parsed file keeps the engine's answer (KT-78).
- `symbols` candidates, including the ambiguous-name listing shared by `trace` and `context`, are printed in a stable order: qualified name, path, line (KT-81).
- The `trace` Usages heading now accounts for every site. When sites are left out by the per-file limit or as imports, the section says how many and why (KT-82).
- Concurrent `daemon start` calls decide who spawns with one atomic claim, so two starts can no longer both report success (KT-71).
- The MCP server instruction string names both version-guarded tools (KT-70).

### Added

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
- `trace` callers can include references in comments and strings (KT-83).
- On Linux hosts below glibc 2.28 the bundled engine does not start. Set `KTSENSE_LSP_PATH` to a host-built `kmp-lsp` 0.26.0.
- In a like-for-like benchmark on ktor (KT-76), grep was faster on every question. ktsense's advantage is structured, smaller answers for questions such as callers of a nested type, not speed.
