# ktsense

Agent-first Kotlin code understanding. `ktsense` is a Rust CLI and a Model Context Protocol (MCP)
server that answer structural questions about a Kotlin or Kotlin Multiplatform workspace in compressed
form, so a terminal AI coding agent spends its context on the answer instead of on the file. It wraps
the upstream Kotlin language server [`kmp-lsp`](https://github.com/Hessesian/kmp-lsp) engine by
process; it does not link it.

A file's API surface is one `outline` call rather than a whole file in the prompt. "Who implements
this interface" is one `trace` call rather than a grep and a page of false positives. An unfamiliar
repository is one token-budgeted `map`. The nine MCP tools cover outlines, symbol lookup, call and
implementor tracing, an import and dependency graph, a repository map, a syntax check, and an
attributed text search.

**What it is not.** Resolution is syntactic. ktsense parses with tree-sitter and asks an engine that
indexes references; it does not type-check. It cannot tell you which overload a call site resolves
to, whether a type argument is valid, or whether the code compiles. Type errors are Gradle's job.
When ktsense is unsure which declaration you meant it lists every candidate rather than guessing, and
answers that depend on the reference index carry a marker saying how complete that index was.

## When to use it, and when `grep` is the better tool

This section exists because the project's own agent evaluation found that **ktsense did not beat a
`grep`-equipped baseline**, and the reason was the question set rather than the tools. Both arms
answered 9 of 9 graded questions correctly; the ktsense arm used more tool calls, 21 against 17, and
was nominally about a third slower at the median, inside this host's noise. Nine of the ten questions
asked where a distinctively named declaration lives, and a single `grep` answers that exactly. So
there is no speed claim here in either direction, and an agent that reaches for ktsense on every
question will do more work than one that picks. The numbers and their caveats are in
[Measured numbers](#measured-numbers) and [bench/agent-eval/results.md](bench/agent-eval/results.md).

**Reach for `grep` or `rg` when you already know a distinctive literal token.** `public interface
CoroutineScope`, an error string, an annotation name. A text search lands on it in one call with no
false positives, and nothing structural improves on an exact match.

**Reach for ktsense when the question is structural, or when the answer would cost more context than
it is worth.** The measured case for it is size rather than speed: an `outline` skeleton of a real
corpus retains 10.63% of raw bytes on kotlinx.coroutines and 14.65% on ktor, so the API surface of a
file or a module arrives without the bodies. Specifically:

| Question shape | Better tool | Why |
|---|---|---|
| One literal you can spell, in a file you already know | `rg -n` | replay: 62 to 317 B where the smallest ktsense answer was about 240 B |
| Where is this exactly-named declaration | `grep`, or `symbols` when the name is common | measured: one call, no false positives; `symbols` qualifies every candidate |
| Several terms or call sites at once | `ktsense grep 'a\|b' --path <dir>` | hits grouped by file and enclosing declaration across Kotlin and Java, count equals `rg -c`; pass `--path`, because unscoped answers ran 3x to 8x the size of a scoped `rg` |
| One guard, branch or line inside a long body | `context Type.member --only source --match <regex>` | the matched lines, numbered, under one location line |
| Who calls this, or what breaks if it changes | `trace Type.member` | callers by enclosing declaration, production before tests; the dotted name avoids the ambiguous-name round trip |
| What extends or implements this type, Java included | `trace <Type>` | direct and transitive subtypes with `via <parent>` |
| What does this file declare, without bodies | `outline` (`--annotations` for DI wiring, `--private` for internals) | measured 85% to 89% fewer bytes than the source |
| A partial name, such as every `*Handler` or a `customerId` property | `symbols --contains` | ranked, capped, constructor properties included |
| Files of a kind, or declarations matching a name | `map --compact --path <segment>`, `map --compact --focus <regex>` | stops at what matched rather than filling the budget |
| Orient me in an unfamiliar repository | `map` | one answer inside a token budget you set |
| What imports what, and are there cycles | `deps` | an import graph, not a pile of matches |
| A name nothing here declares: a library call, a Lombok accessor, a generated wrapper | `trace <name>` | text references across Kotlin and Java, each under its enclosing declaration; build first if the code is generated |
| This name is ambiguous | `symbols`, `trace` | every candidate is listed; paste the suggested `--pick` on its own |
| Did my edit parse | `check` | the engine's own checker, before the next build |

The rows marked replay come from re-running all 107 questions real agent sessions had recorded
through `kt-bench`, both arms, on build 6e747a5 (2026-10-04). With the command each session had
chosen, ktsense won 44, tied 23 and lost 37 (3 neither). 28 of those losses were a command that could
not answer the question; with the command this table now names, 9 became ktsense wins and 5 ties.

Two honest limits on that table. The evaluation did **not** test the budgeted map, the ranked outline
of an unfamiliar module, or a symbol ambiguous enough that a text search returns a hundred candidates,
which are exactly the rows a structural answer should win; those rows rest on the compression and
latency measurements plus the tool contracts, not on a head-to-head result. And nothing in the
evaluation measured context window consumption, which is the reason this tool exists.

**If you are configuring an agent, install the skill file.** On 3 of the 10 evaluation questions the
ktsense-equipped arm ignored ktsense and reached for `grep`, having been told nothing about when to
prefer which tool and with no skill file installed. That is a discoverability finding, not a tool
quality one. `contrib/agent-skill/SKILL.md` carries this routing in the form an agent reads, and a
release tarball ships it at the archive root as `SKILL.md`.

On a host where the bundled engine cannot run, `outline`, `deps`, `map`, `grep` and `status` still
answer and
the engine-backed rows above do not. See [Requirements](#requirements).

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

### One script, if you would rather not run the steps yourself

`scripts/install.sh` does the whole Homebrew path and then proves the result by running the binary:

```
git clone https://github.com/siddharthasikder/ktsense
ktsense/scripts/install.sh
```

It finds Homebrew or offers to install it, puts it on the current shell's PATH, installs `ripgrep`,
taps, asks for tap trust, installs the formula, and finishes by running `--version`, an `outline` over
a file it generates, and `status`, so a pass means the binary answered rather than that a command
exited zero.

Two things it deliberately does not do. **It never grants tap trust silently.** The decision above is
yours, so the script prints what is about to be trusted and stops unless you pass `--trust-tap` or
confirm at the prompt. **It never installs Homebrew behind your back**, for the same reason:
`--bootstrap-brew` or a confirmation is required, and without either it exits with instructions.
`--yes` supplies both consents at once for an unattended run, and `--help` lists the rest.

It installs `ripgrep` even though the formula does not require it, because the engine execs `rg` for
reference and declaration search and answers nothing without it, which reads as "no such symbol"
rather than as an error. `--skip-ripgrep` accepts that degradation knowingly.

On a Linux host below glibc 2.28 the script warns and continues rather than refusing, because the
tree-sitter commands work there and the engine-backed ones do not; the install is still worth having,
and the warning names which is which.

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
| `map` | Token-budgeted map of the most central files (`--budget`, default 4000; `--compact` for names only; `--compact --focus <regex>` to list matching declarations and stop; add `--fill` to then spend the rest of the budget on the ranked map; `--path <substring>`, repeatable, to map only files whose path contains it) |
| `check <paths>` | Syntax check; exits non-zero when a file has errors |
| `diagnose <file>` | Semantic diagnostics on one file |
| `context <name>` | Budgeted context bundle for one symbol (`--only source,callers,implementors,outline` to restrict it; `--match <regex>` to filter the shown source lines) |
| `grep <regex>` | Text search over Kotlin and Java sources, each hit attributed to its declaration (`--path`, `--tests`/`--no-tests`, `--limit`, `--kotlin-only` for Kotlin alone, and `-w`/`-i`/`-F` like ripgrep) |
| `status` | Index phase, file and symbol counts, engine version |
| `mcp` | Run the MCP stdio server |
| `daemon start\|status\|stop` | Manage the warm-session daemon |

Always pass `--root`, or run from inside the repository you mean. Upstream defaults its workspace root
to the nearest enclosing `.git` directory rather than the working directory, and a root wider than you
intended does not fail, it answers about the wrong code.

A declaration lookup (`symbols` and `symbols --contains`) also reads generated Kotlin under a module's
`build/generated` and `build/generated-src`, which the engine does not index, and labels those rows
`generated`. An exact `symbols <name>` the engine answers with nothing falls back to the workspace's
own syntax index, stating `source: syntax index`. `map` and `outline` keep their build-free view. When
a lookup finds nothing, the answer says whether generated sources were searched or are absent and the
build should be run.

`--format json` is for tool chaining; the default Markdown is shaped for prompt injection.

### The warm daemon

```
ktsense daemon start --root /path/to/repo
```

This keeps an engine session warm for one root. `outline`, `deps`, `map`, `trace` and `context` route
through it automatically when it is live and fall back to running in process when it is not, so a
routing problem degrades to a slower answer rather than no answer. A daemon you start by hand expires
after an hour idle; `--idle <minutes>` sets a different window, and `daemon status` shows the window a
running daemon holds. Nothing requires it; see the latency table for what it is worth per command.

You rarely need to start it by hand. The first `trace` or `context` in a root, finding no daemon,
answers in process and then starts one in the background, so the next question is answered warm
(about 50 ms against 0.7 to 2 s). The first answer is unchanged and the background start never blocks
or fails it; concurrent first uses settle on a single daemon. A daemon started this way idles out
after fifteen minutes, sooner than a hand-started one, since a session is often done with a root
within the hour. Set `KTSENSE_NO_AUTOSTART=1` to answer in process without starting one, or
`KTSENSE_NO_DAEMON=1` to also skip routing through an existing one.

After an upgrade, a daemon an older build left warm speaks an older protocol. `daemon stop` stops a
daemon of any protocol version, `daemon status` names both versions when they differ, and the next
engine-backed command's background start replaces the mismatched daemon with a current one, so an
upgrade never strands a root without a warm daemon.

### As an MCP server

`ktsense mcp` speaks JSON-RPC over stdio and exposes nine tools: `get_kotlin_outline`,
`find_kotlin_symbol`, `trace_kotlin_symbol`, `analyze_kotlin_dependencies`, `get_kotlin_repo_map`,
`check_kotlin_syntax`, `explain_kotlin_symbol`, `ktsense_status` and `search_kotlin_text`.

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
every candidate and exits 3 rather than picking one, ending with a `--pick` hint it also writes to
stderr; three of nine here with their paths shortened:

````
$ ktsense trace Plugin
## Ambiguous: Plugin (9 candidates)

```text
io.ktor.client.plugins.DefaultRequest.Plugin object  .../DefaultRequest.kt:63 companion object Plugin : HttpClientPlugin<DefaultRequestBuilder, DefaultRequest>
io.ktor.server.application.Plugin            interface .../ApplicationPlugin.kt:21 interface Plugin<in TPipeline : Pipeline<*, PipelineCall>, out TConfiguration : Any, TPlugin : Any>
io.ktor.server.websocket.WebSockets.Plugin  object  .../WebSockets.kt:111  companion object Plugin : BaseApplicationPlugin<Application, WebSocketOptions, WebSockets>
...
```

ambiguous: 9 declarations named Plugin; rerun with --pick DefaultRequest.Plugin
````

Retry with `--pick <fully.qualified.name>`, or the shortest dot-boundary suffix that names one
candidate, such as `--pick DefaultRequest.Plugin`. The pick stands alone as the query:
`ktsense trace --pick DefaultRequest.Plugin` needs no positional, taking the lookup name from the
pick's last dot segment, so the hint can be rerun as written.

Or name the member in the query itself: `symbols`, `trace` and `context` accept a dotted
`Type.member`, so `ktsense trace DefaultRequest.Plugin` looks up `Plugin` and keeps only the
candidate whose fully-qualified name the whole query is a dot-suffix of, answering directly where the
bare name is ambiguous.

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

**A name the workspace does not declare still gets an answer.** When `trace` or `context` resolves no
declaration named X, it lists where X is written in the workspace's Kotlin sources under `## Text
references (N sites in M files)`, grouped by file, capped by `--limit`, and marked `precision: text
match` so it is never read as resolved usages; comment and string mentions are listed but counted
apart from code. The scope wording stays and the exit stays 1, but the listing is the answer, so a
library method such as `putMetric` or an annotation from a dependency points at its use sites instead
of at nothing.

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
