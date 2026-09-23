# ktsense

Agent-first Kotlin code understanding. `ktsense` is a Rust CLI and an MCP server that answer
structural questions about a Kotlin workspace in compressed form, so a terminal AI agent spends its
context on the answer instead of on the file. It wraps the upstream
[`kmp-lsp`](https://github.com/Hessesian/kmp-lsp) engine by process; it does not link it.

A file's API surface is one `outline` call rather than a whole file in the prompt. "Who implements
this interface" is one `trace` call rather than a grep and a page of false positives. An unfamiliar
repository is one token-budgeted `map`.

**What it is not.** Resolution is syntactic. ktsense parses with tree-sitter and asks an engine that
indexes references; it does not type-check. It cannot tell you which overload a call site resolves
to, whether a type argument is valid, or whether the code compiles. Type errors are Gradle's job.
When ktsense is unsure which declaration you meant it lists every candidate rather than guessing, and
answers that depend on the reference index carry a marker saying how complete that index was.

## Install

### Prerelease tarballs (current path)

`v0.0.1-rc.2` is published with four platform artifacts. It is a release candidate for the release
dry run, not a general-use release.

```
https://github.com/siddharthasikder/ktsense/releases/tag/v0.0.1-rc.2
```

| Platform | Asset |
|---|---|
| macOS arm64 | `ktsense-0.0.1-rc.2-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `ktsense-0.0.1-rc.2-x86_64-apple-darwin.tar.gz` |
| Linux arm64 | `ktsense-0.0.1-rc.2-aarch64-unknown-linux-musl.tar.gz` |
| Linux x86_64 | `ktsense-0.0.1-rc.2-x86_64-unknown-linux-musl.tar.gz` |

Each asset ships with a `.sha256` sidecar. Verify before unpacking. The archive holds `bin/ktsense`
beside `libexec/kmp-lsp`, plus `SKILL.md`, `LICENSE` and `LICENSE.kmp-lsp` at its root. Keep `bin/`
and `libexec/` in their relative places: that is how the binary finds its bundled engine with nothing
configured.

**A Linux host needs glibc 2.28 or newer for the bundled engine.** `bin/ktsense` itself is
musl-static and runs below that: measured from this tarball on Amazon Linux 2 (glibc 2.26, aarch64),
`outline`, `deps` and `map` answer normally and `status` reports the engine as unavailable with an
actionable message. `libexec/kmp-lsp` is glibc-linked and the loader refuses it below `GLIBC_2.28`,
so `symbols`, `trace`, `check`, `diagnose`, `context` and the daemon fail there, each printing the
loader's own line; `daemon start` reports only that nothing answered on its socket, after burning its
full 30 s timeout, rather than naming the engine. `readelf -V` on the arm64 engine gives 2.28 as its
highest version reference, from the single symbol `statx`, and the companion `libexec/kmp-jar-indexer`
records a higher floor of `GLIBC_2.34`. On an older host, build an engine for it and point
`KTSENSE_LSP_PATH` at that. macOS is unaffected (KT-72).

### Homebrew

```
brew tap siddharthasikder/ktsense
brew trust siddharthasikder/ktsense
brew install ktsense
```

**The middle step is not optional on current Homebrew, and it is the step the two-command version of
these instructions used to omit.** Measured on Homebrew 7.0.6: the tap clones, and then an
unqualified `brew install ktsense` refuses the formula as untrusted and names the two ways to grant
trust, either `brew trust --formula siddharthasikder/ktsense/ktsense` for this formula alone or
`brew trust siddharthasikder/ktsense` for the whole tap. After that the same unqualified install
succeeds, fetching the published `v0.0.1-rc.2` artifact, and `brew test` and `brew audit --strict`
both pass (KT-45).

That refusal is Homebrew's own safety decision about non-official taps, not a ktsense requirement.
Granting trust says you have read `Formula/ktsense.rb` and the repository serving it and accept what
they will run on your machine, so read them first. The record is written to `~/.homebrew/trust.json`,
or under `$XDG_CONFIG_HOME/homebrew/` when that variable is set.

Removing it all again, trust record included:

```
brew uninstall ktsense
brew untrust --tap siddharthasikder/ktsense
brew untap siddharthasikder/ktsense
```

A prerelease tarball remains the alternative if you would rather not add a tap at all.

### From source

```
cargo build --release -p ktsense-cli     # leaves target/release/ktsense
```

A source build does not bring the engine with it. Install `kmp-lsp` yourself, or point
`KTSENSE_LSP_PATH` at it.

## Requirements

**The `kmp-lsp` engine**, version 0.26.0, for every command that reaches the engine: `symbols`,
`trace`, `check`, `diagnose` and `context`. `outline`, `deps` and `map` are pure tree-sitter and need
nothing, and `status` runs either way, reporting the engine as unavailable rather than failing. Note
that `check` needs the engine even though it does not wait for an index: it is a passthrough to the
engine's own syntax checker. ktsense looks for the engine in three places in order:
`KTSENSE_LSP_PATH`, then `libexec/kmp-lsp` beside the binary's own directory, then `kmp-lsp` on
`PATH`. An override naming a file that does not exist does not fail; discovery falls through to
`PATH`, so a typo silently gets you whichever engine is installed. Discovery finding the engine is not
the same as this host being able to run it: a Linux host whose loader refuses the bundled,
glibc-linked engine behaves like a host with none installed, and `status` reports an unrunnable engine
as unavailable exactly as it reports a missing one. The glibc note in Install above is the measured
account of which commands still answer there.

**ripgrep.** This one is easy to miss because nothing announces it. The engine's reference search
execs `rg`, and without it the search does not fail, it answers nothing. Measured on
`fixtures/multi-module` with the index complete: `refs` reports 6 reference sites with `rg` on `PATH`
and 0 without, and a `trace` answers `Callers (3)` and 6 usages in 6 files with it against
`Callers (0)` and 1 usage in 1 file without, identically through the warm daemon and in process. A
declaration search on a cold cache behaves the same way: 3 declarations with `rg`, 0 with only `fd`, 0
with neither. Since an empty result is also what "no such symbol" looks like, **a missing `rg` is
indistinguishable from an absent symbol.** Install it (KT-67). `outline` and `map` are unaffected.

## Using it

```
ktsense outline src/main/kotlin/app/service/UserService.kt
```

````
## src/main/kotlin/app/service/UserService.kt

package app.service

```kotlin
open class UserService(private val repo: UserRepository, private val clock: Clock) : Service {
    override val name: String
    val cachedCount: Int
    fun createUser(email: String, displayName: String? = null): Outcome<User>
    suspend fun findAll(page: Int = 0): List<User>
    suspend fun promote(id: UserId, role: Role): Outcome<User>
    protected open fun onEviction(user: User)
    companion object {
        const val MAX_PAGE_SIZE: Int
        fun describe(): String
    }
}
```
````

Sixty-six lines of source, twelve lines of signatures. That is the whole idea.

| Command | Answers |
|---|---|
| `outline <file>` | Compressed declaration skeleton of one file |
| `symbols <name>` | Declarations matching a name across the workspace |
| `trace <name>` | Definition, usages, implementors and callers of one symbol |
| `deps` | Import graph of the workspace, including cycles (`--format dot` for Graphviz) |
| `map` | Token-budgeted map of the most central files (`--budget`, default 4000) |
| `check <paths>` | Syntax check; exits non-zero when a file has errors |
| `diagnose <file>` | Semantic diagnostics on one file |
| `context <name>` | Budgeted context bundle for one symbol |
| `status` | Index phase, file and symbol counts, engine version |
| `mcp` | Run the MCP stdio server |
| `daemon start\|status\|stop` | Manage the warm-session daemon |

Always pass `--root`, or run from inside the repository you mean. Upstream defaults its workspace root
to the nearest enclosing `.git` directory rather than the working directory, and a root wider than you
intended does not fail, it answers about the wrong code.

`--format json` is for tool chaining; the default Markdown is shaped for prompt injection.

### The warm daemon

```
ktsense daemon start --root /path/to/repo
```

This keeps an engine session warm for one root. `outline`, `deps`, `map` and `trace` route through it
automatically when it is live and fall back to running in process when it is not, so a routing problem
degrades to a slower answer rather than no answer. It expires after an hour idle. Nothing requires it;
see the latency table for what it is worth per command.

### As an MCP server

`ktsense mcp` speaks JSON-RPC over stdio and exposes eight tools: `get_kotlin_outline`,
`find_kotlin_symbol`, `trace_kotlin_symbol`, `analyze_kotlin_dependencies`, `get_kotlin_repo_map`,
`check_kotlin_syntax`, `explain_kotlin_symbol` and `ktsense_status`.

[docs/agents.md](docs/agents.md) wires it into Kiro CLI, Claude Code and Codex, and shows how to prove
the server works before involving a client at all. [docs/mcp-tools.md](docs/mcp-tools.md) is the
contract the tool descriptions hold themselves to. `contrib/agent-skill/SKILL.md` teaches an agent
when to reach for which tool and is worth installing alongside the server; a release tarball carries
it as `SKILL.md`.

## Measured numbers

Every number below was measured on this project's own benchmark scripts. Nothing here is a target or
an estimate except the token counts, which say so.

### Compression

How much of a Kotlin source tree survives as an `outline` skeleton. Lower is better.

| Corpus | `.kt` files | Raw bytes | Skeleton bytes | Est. tokens | Skeleton % of raw | Reduction |
|---|---|---|---|---|---|---|
| kotlinx.coroutines 1.9.0 | 1025 | 3,835,333 | 407,557 | 113,211 | **10.63%** | 89.37% |
| ktor 3.0.1 | 1861 | 6,052,579 | 886,970 | 246,381 | **14.65%** | 85.35% |
| `fixtures/tiny-app` | 15 | 13,432 | 6,247 | 1,736 | 46.51% | 53.49% |

*Provenance: `bench/compress.sh <corpus> --max-percent 30`, measured on a Linux aarch64 dev host
(32 cores) on 2026-09-22, ktsense built from `2d1e8db` in release profile. Corpora are the pinned
clones `bench/clone.sh` restores: kotlinx.coroutines tag `1.9.0` at `d8d6f8f`, ktor tag `3.0.1` at
`205479f`. The 30% gate passed on both real corpora. A second run of the kotlinx.coroutines
measurement was byte-identical.*

Token counts are an estimate, `ceil(skeleton bytes / 3.6)`, which is the product's own byte-ratio
estimator and not a tokenizer count.

What is in the totals, so the percentage means something. Both corpora had **zero files rejected**:
five files across the two (four in kotlinx.coroutines, one in ktor) have a localized parse error and
were recovered partially rather than dropped, and their bytes are inside both the raw and the skeleton
totals like any other measured file. `.kts` Gradle scripts are excluded from both totals, since they
declare nothing and counting their raw bytes would pad the denominator. Generated and tooling trees
(`build`, `bin`, `target`, `out`, and friends) are pruned so a generated copy cannot double-count. A
rejected file, had there been one, would be excluded from both totals and listed by name in the
report.

`fixtures/tiny-app` retains 46.51%, and that is honest rather than embarrassing. It is a 15-file
grammar and rendering fixture, deliberately signature-dense: almost every line is a declaration and
bodies are one-liners, so there is very little for a skeleton to remove. It is coverage, not a corpus,
which is why it carries no under-30% gate. Reading it as a compression result was the mistake KT-11
corrected, and quoting only the two real corpora would be the same mistake told quietly.

### Latency

Per-command wall time on ktor, comparing the in-process path against a warm daemon.

| Command | In process (median) | Warm daemon (median) |
|---|---|---|
| `outline` (54 KB file) | 17 ms | **3 ms** |
| `symbols` | 289 ms | not routed |
| `trace` | 1159 ms | **165 ms** |
| `map` | 2194 ms | 2276 ms |
| `deps` | 1714 ms | 204 ms |

*Provenance: `bench/latency.sh bench/repos/ktor --reps 9`, measured on a Linux aarch64 dev host
(32 cores) on 2026-09-22, ktsense built from `2d1e8db` in release profile, engine `kmp-lsp 0.26.0`.
ktor tag `3.0.1` at `205479f`, 1861 `.kt` files, and clean: zero duplicate sources under `bin/`, so
no row is inflated by a Buildship import. The daemon was started and confirmed at `index: complete`
before timing, so the daemon column is warm-session time. One minute load average ranged 1.57 to 2.17
across the rows. A separate five-repetition run produced the same ordering; median differences were 0-9 ms on every row except daemon map, which differed by 86 ms (2190 versus 2276 ms).*

Medians over nine timed repetitions after one discarded warm-up invocation, taken with the bash `time`
keyword so nothing forks inside the measured span. `outline` is the corpus's largest file, 54 KB, so
its number is a worst case rather than a typical one.

Read the rows honestly, because they do not all point the same way.

The 3 ms routed `outline` is a warm-cache number. A routed `outline` is answered from a
fingerprint-guarded skeleton cache in the daemon, so the discarded warm-up populates the cache and the
timed repetitions are hits. The first call on a file, or the first after that file changes, pays the
17 ms parse.

`trace` is where the daemon earns its keep, at 165 ms against 1159 ms, because it answers on a session
that is already open with an index already built instead of launching an engine child per call.

`map` is **slower** through the daemon than in process, 2276 ms against 2194 ms. It is pure
tree-sitter work over the whole tree, so routing it buys nothing and costs a round trip. The row stays
in the table.

`symbols` has no daemon column because it is not routed, so there is no daemon number to report. Its
in-process 289 ms is worth reading next to `trace`: it is one command-mode declaration search, which
is the subprocess a routed `trace` used to pay to resolve its symbol and now answers from the daemon's
warm index instead (KT-60).

**Cold index.** The daemon rows above are warm, so here is the cost of warming, measured against a
provably empty cache: ktor indexed 1861 files and 29,575 symbols in **1.04 s** from cold and 0.71 s
from the cache that run wrote; kotlinx.coroutines indexed 1044 files and 17,181 symbols in 0.89 s from
cold. *Provenance: `bench/cold-index.py bench/repos/ktor bench/repos/kotlinx.coroutines`, same host
and date. Coldness is proven by the engine's own status: `cache_hits: 0` on the cold run, equal to the
file total on the warm one. The file counts are the engine's own and prune less than `compress.sh`
does, which is why kotlinx.coroutines reads 1044 here and 1025 in the compression table. This does not
control the operating system's page cache, so a first run after a reboot may read slower.*

### Agent evaluation

Ten questions about `kotlinx.coroutines` asked of two Kiro CLI agents differing in exactly one
respect. The baseline has `read`, `grep`, `glob`, `ls` and `shell`. The second arm has those five plus
the eight ktsense MCP tools. So this measures what ktsense adds to an agent that can already search a
repository.

| Arm | Sessions | Graded | Correct | Wrong | Median wall | Tool calls | of which ktsense |
|---|---|---|---|---|---|---|---|
| baseline (grep) | 10 | 9 | 9 | 0 | 11.0 s | 17 | 0 |
| ktsense | 10 | 9 | 9 | 0 | 14.6 s | 21 | 9 |

*Provenance: `bench/agent-eval/run.sh --root bench/repos/kotlinx.coroutines`, run 2026-09-21 on a
Linux aarch64 dev host, ktsense built from `349f300`, engine `kmp-lsp 0.26.0`, `kiro-cli 2.22.1`,
corpus `kotlinx.coroutines` at `d8d6f8f`. Full write-up, per-question table and method in
[bench/agent-eval/results.md](bench/agent-eval/results.md) (KT-39). Not re-measured for this README;
that run is 20 sessions and about 11.45 credits.*

**ktsense did not win.** It matched the grep baseline on correctness, 9 correct out of 9 graded
questions on both arms, and took roughly a third more wall time at the median while using more tool
calls, 21 against 17. One question is ungraded because its ground truth was wrong and both arms were
right.

**There is no speed claim here, in either direction.** Three of the ten questions are an accidental
null control, because on those the ktsense-equipped arm reached for `grep` and used no ktsense tool at
all, so both arms ran identical tooling; on those three the ktsense arm came out between 7.0 s faster
and 5.7 s slower. A 12.7 s noise band around zero swallows the 3.6 s median difference, so that
difference is unresolved at one session per cell rather than a measured cost. The span is dominated by
model latency, not tool latency.

The reason the evaluation is a tie is visible in the questions rather than in the tools. Nine of ten
ask where a named declaration lives or what it declares, and `kotlinx.coroutines` gives almost every
declaration a distinctive name, so a single grep lands on the answer with no false positives. There is
nothing for a structural tool to improve on when a text search is already exact. The question set does
not test a budgeted repository map, a ranked outline of an unfamiliar module, or a symbol ambiguous
enough that a text search returns a hundred candidates, which are the cases a compressed structural
answer should win. Nor does anything there measure context window consumption, which is ktsense's
stated reason for existing. A set built to discriminate is follow-up work, not a claim this README
gets to make.

## Accuracy and honesty

**Resolution is syntactic, not type-checked.** Type errors are Gradle's job. A clean `check` means the
file parsed, not that it compiles.

**Ambiguity is an answer, not a guess.** When a name resolves to several declarations, ktsense lists
every candidate and exits 3 rather than picking one, three of nine here with their paths shortened:

````
$ ktsense trace Plugin
## Symbols: Plugin

```text
io.ktor.server.websocket.WebSockets.Plugin  object  .../WebSockets.kt:111  companion object Plugin : BaseApplicationPlugin<Application, WebSocketOptions, WebSockets>
io.ktor.client.plugins.DefaultRequest.Plugin object  .../DefaultRequest.kt:63 companion object Plugin : HttpClientPlugin<DefaultRequestBuilder, DefaultRequest>
io.ktor.server.application.Plugin            interface .../ApplicationPlugin.kt:21 interface Plugin<in TPipeline : Pipeline<*, PipelineCall>, out TConfiguration : Any, TPlugin : Any>
...
```
````

Retry with `--pick <fully.qualified.name>`.

**Output carries its own precision level.** A `trace` says how complete the index was when it
answered, and says where its callers came from:

```
# Trace: app.service.UserService

index: complete
...
Callers are the declarations enclosing each reference site; the engine reports no call hierarchy. Resolution is syntactic, not type-checked.
```

`index: partial` means the reference index had not settled. It is a real answer about what was
indexed, not a complete one. Ask again, or pass `--wait-index`.

**Exit codes are a contract an agent can branch on.**

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | The operation failed on its input: an unreadable path, a directory, a broken Kotlin file |
| 2 | The invocation itself was malformed |
| 3 | The symbol name resolved to several candidates; pick one and retry |
| 70 | The subcommand exists in the surface but has not shipped |

Code 3 is distinct from 2 on purpose. "I called the tool wrong" and "the name was ambiguous" demand
opposite responses. No command returns 70 today, since `context` was the last one the surface
advertised without implementing; the code stays in the contract for the next surface-first command.

## Reproducing the numbers

```
cargo build --release -p ktsense-cli
bash bench/clone.sh                                                  # pinned corpora into bench/repos
bash bench/compress.sh bench/repos/kotlinx.coroutines --max-percent 30
bash bench/compress.sh bench/repos/ktor --max-percent 30
bash bench/compress.sh fixtures/tiny-app                             # report only, no gate
ktsense --root "$PWD/bench/repos/ktor" daemon start                  # wait for index: complete
bash bench/latency.sh bench/repos/ktor --reps 9
python3 bench/cold-index.py bench/repos/ktor
bench/agent-eval/run.sh --root bench/repos/kotlinx.coroutines        # 20 sessions, spends credits
```

`bench/clone.sh` is the only script that touches the network. `bench/repos/` is gitignored; corpora
are cloned, never vendored. Time the latency benchmark on a corpus the engine has not yet imported:
opening a Gradle project triggers a Buildship import that writes byte-identical `bin/` copies of the
sources, and a doubled tree inflates any command that walks it. The harness reports the duplicate
count so you can tell.

Your numbers will differ from the table. Publish what you measure.

## Repository layout

| Crate | Role |
|---|---|
| `ktsense-core` | Pure domain: skeleton model, compressors, ranking, budgeting |
| `ktsense-syntax` | tree-sitter Kotlin to core values |
| `ktsense-lsp` | Client for the `kmp-lsp` child process |
| `ktsense-daemon` | Warm session over a Unix socket |
| `ktsense-mcp` | MCP stdio server |
| `ktsense-cli` | The `ktsense` binary |

`ktsense-core` depends on `serde` and nothing else: no filesystem, no process, no runtime, no parser.
That is what keeps ranking, budgeting and compression testable from hand-built values.

`cargo test` needs no engine install. The adapter tests drive a fake `kmp-lsp` replay binary; tests
against the real engine are behind the `real-lsp` feature. [AGENTS.md](AGENTS.md) carries the
conventions, and the measured upstream behaviours worth knowing before you touch the engine path.

## License

MIT, in [LICENSE](LICENSE). The bundled `kmp-lsp` engine is MIT as well, vendored at
[LICENSE.kmp-lsp](LICENSE.kmp-lsp).
