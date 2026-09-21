# MCP tools

`ktsense mcp` serves eight tools over stdio. This page is the contract between the catalogue in
`crates/ktsense-mcp/src/lib.rs`, the descriptions an agent reads, and the structured half of every
result. The pinned wire form of the catalogue is
`crates/ktsense-mcp/tests/snapshots/tools_list__tools_list.snap`, which is the exact
`tools/list` payload a client receives.

## What a description has to say

A description is not documentation for a human who already decided to read it. It is the only thing
an agent sees when it decides whether to call the tool at all, so each one answers four questions in
this order:

1. **What question does this answer?** Phrased as the question, not as the implementation.
2. **When should it be preferred over the obvious alternative?** Usually `grep`, sometimes reading
   the file, sometimes another ktsense tool.
3. **What does it require?** `requires: nothing`, `requires: kmp-lsp`, or
   `requires: kmp-lsp and a settled index`.
4. **What does it cost?** A measured figure with the corpus it was measured on, or the word
   `unmeasured`.

## `requires:` has three states, not two

The marker KT-31 shipped was `cost: fast | needs_index`, and both halves of it were wrong.

`fast` was read as "needs no engine". It does not mean that. `check_kotlin_syntax` is a passthrough
to `kmp-lsp check`: it waits for no index, but on a host with no engine installed it fails on every
call. A marker that only distinguishes "index" from "no index" sends an agent down that path and
lets it conclude the tool is broken. `find_kotlin_symbol` is the same shape, over `kmp-lsp find`.

`fast` was also read as "cheap", and KT-38 measured that as false. `analyze_kotlin_dependencies`
needs neither engine nor index and takes about 1.7 seconds on ktor's 1861 files, because it parses
every file in the tree. `get_kotlin_repo_map` is about 1.1 seconds for the same reason. Calling
those fast would be the most misleading thing on the page.

So requirement and cost are now two separate markers. `requires:` names what has to be installed
and settled, in three states; `cost:` carries a measured number or admits there is none.
| Tool | Command | `requires:` | `cost:` | Source |
|---|---|---|---|---|
| `get_kotlin_outline` | `outline` | nothing | ~30 ms, 54 KB file | KT-38 |
| `find_kotlin_symbol` | `symbols` | kmp-lsp | ~300 ms, 1861 files | KT-38 |
| `trace_kotlin_symbol` | `trace` | kmp-lsp and a settled index | ~1.2 s, 1861 files | KT-38 |
| `analyze_kotlin_dependencies` | `deps` | nothing | ~1.7 s, 1861 files | KT-38 |
| `get_kotlin_repo_map` | `map` | nothing | ~1.1 s, 1861 files | KT-38 |
| `check_kotlin_syntax` | `check` | kmp-lsp | ~145 ms, 54 KB file | KT-32 |
| `explain_kotlin_symbol` | `context` | kmp-lsp and a settled index | ~1.1 s, 1861 files | KT-34 |
| `ktsense_status` | `status` | nothing | ~100 ms, 1990 files | KT-32 |

Every KT-38 figure is a median of nine timed repetitions on ktor 3.0.1 from
`.agents/scratchpad/ktsense/KT-38.md`, in-process rather than through a daemon. The KT-32 and KT-34
figures were taken the same way, from a release binary, with the exit status checked so a failing
command cannot be reported as a latency. No figure appears in a description that is not in one of
those evidence files.

The corpus is the same repository in both cases, counted two ways. KT-38 counted 1861 `.kt` files.
`ktsense_status` reports 1990, because its traversal accepts `.kts` too and ktor holds 129 Gradle
Kotlin scripts. Both numbers are right about different questions, so each description states the one
its own measurement walked.

## Version compatibility is not uniform, so the instructions scope it

`check_version` is called from exactly one place, `ktsense_lsp::launch()`, which is the LSP session
path. `EngineCommand`, which backs the engine's command mode, performs no probe. So of the
engine-backed tools only `trace_kotlin_symbol` refuses an incompatible engine major version;
`find_kotlin_symbol` and `check_kotlin_syntax` will run against whatever binary discovery found.
The server `instructions` say exactly that rather than claiming version safety across the board.
`ktsense_status` reports the installed version and its compatibility, and never fails for a missing
engine: absence is the state a caller runs it to learn.

## Needing an index and being shaped by one are different questions

`requires:` says what has to be there before a tool can answer at all. Whether a settled index
changes *what* it answers is a separate fact, held separately in the catalogue as
`index_shapes_answer`, and it was measured rather than assumed.

On ktor with a provably empty engine cache, `find_kotlin_symbol` returns four candidates for one
class, all in the same file at lines 28, 162, 301 and 352, because the engine's `find` falls back to a
text search when it has no index. `trace_kotlin_symbol` then exits ambiguous in 85 ms instead of
answering. Against a settled index both are exact: one candidate, and a trace marked
`index: complete`. So `find_kotlin_symbol` waits for no index, which is what `requires: kmp-lsp` says,
and is still shaped by one, which its description now warns about. `check_kotlin_syntax` is the other
side of that line: it needs the engine, parses one file, and an index would change nothing.

The distinction is what decides which tools warm a root, below.

## Roots, and which workspace a call is about

Three sources, in order:

1. The call's own `root` argument. The caller was specific, so nothing overrides it.
2. The roots the client advertised through `roots/list`: the first one that holds the file or
   directory the call names, else the first one it listed.
3. Neither, in which case the root the server was started with is used: `--root`, and failing that
   the working directory. That fallback already lives in the CLI and is not duplicated here.

Picking the root that holds the requested file is not a nicety. AGENTS.md records that a widened root
does not fail, it answers about the wrong code, and an editor with two projects open advertises both.
Containment is decided against the filesystem: an absolute subject by prefix, resolving symlinks when
a textual compare would miss, and a relative one by whether it actually exists under that root, since
the same relative path is valid under several roots at once.

The server asks for roots inside `call_tool`, before dispatching, rather than from the `initialized`
notification. rmcp handles notifications in their own task, which a tool call arriving straight
afterwards can overtake, so fetching there would make the choice of root a race. It asks once, caches
the answer, and drops the cache when the client sends `roots/list_changed`. A client that never
declared the roots capability is never asked.

`Peer::list_roots` is deprecated in rmcp 2.2.0, because SEP-2577 deprecates roots protocol-wide. It
is still what every client that has roots speaks, so it is used, with the deprecation allowed at that
one call site and nowhere else.

## The warm engine, and what holding one actually buys

When a root's index would shape an answer and no daemon is holding a session for it, the MCP server
opens one `kmp-lsp` session itself and keeps it alive until it stops serving. Tool calls are still
answered by a delegated `ktsense` child; the held session answers nothing. What it does is build the
workspace index, which the children then read from the engine's on-disk cache.

That distinction matters, because the benefit is not the one you would guess. Measured on ktor with a
provably empty cache:

| | first call | second | third |
|---|---|---|---|
| through the MCP server, holding a warm engine | 2347 ms, `index: complete` | 1178 ms | 1180 ms |
| the same `trace` straight from the CLI | 85 ms, **exit 3 ambiguous** | 85 ms, ambiguous | 85 ms, ambiguous |

The CLI arm never answers. Its `find` text-searches a cold index, gets four candidates, and exits
ambiguous every time, because nothing in that arm ever builds the index. The warm session builds it
once, 1861 files and 29,575 symbols, and every call after that is exact. Running the same three CLI
traces against the cache the warm session left behind gives `index: complete` in 1.18 s each.

So the first index-shaped call on a new repository pays about a second for the warm-up, and
everything after it is both faster and correct. On a tiny tree the warm-up is a pure cost: on the
9-file fixture the same experiment gives 1201, 567, 565 ms through the server against 671, 567, 568 ms
from the CLI, because a 9-file text search resolves correctly anyway. The benefit scales with the
repository, which is the case an agent meeting an unfamiliar codebase is actually in.

Warming never fails a call. A launch that fails or exceeds its 30 second bound is logged and the call
is delegated anyway, from a cold index; that bound covers the daemon probe as well as the warm-up,
since the probe runs a `status` command as a child and nothing else would stop a wedged one holding a
tool call open. Roots are decided independently: the lock covers the claim on a root and nothing after
it, so a second workspace's first index-shaped call does not queue behind a first workspace's warm-up. Every held session is shut down when the server stops
serving, which was checked by counting `kmp-lsp` processes before and after: none is left behind.
`KTSENSE_MCP_NO_WARM_ENGINE=1` turns warming off, for the small-tree case above and for an operator
who wants the server to spawn no engine but the ones its commands spawn themselves.

`check_kotlin_syntax` deliberately does not warm anything. It is the tool an agent calls after every
edit, its answer does not depend on an index, and making the first check after a change pay a session
launch would be a straight regression on the loop it exists for.

## `structuredContent`

Every result carries the command's Markdown as its text content and an `Answer` object as its
`structuredContent`, declared by an output schema identical across all eight tools:

| Field | Meaning |
|---|---|
| `tool`, `command` | Which tool answered and the `ktsense` subcommand it delegated to. |
| `requires` | The same three-state marker as the description. |
| `root` | The workspace root the answer is about, which the cited paths are relative to. |
| `exit` | The command's exit status: `0` an answer, `3` a name that resolved to several candidates, `1` a failure on the input, `2` a malformed invocation. |
| `index` | `complete` or `partial`, when the answer carried an index marker. |
| `warmth` | What the server did about keeping an engine warm for this root, when the tool is one an index shapes. |
| `files` | Files the answer is about, in the order it introduced them. |
| `citations` | Every `path`, `line` and optional `column` the answer cites. |
| `citations_omitted` | How many further citations the text has and this index does not. |

`warmth` is the one field that is not read out of the answer text, and it is there for a reason the
reviewers put their finger on. `find_kotlin_symbol` carries no `index:` marker, and its answer is
shaped by the index anyway: against a cold one the engine text-searches and a single declaration can
arrive as several candidates that look exactly like a genuine ambiguity. So the server reports what it
itself did, `opened`, `left_to_daemon`, `already_decided` or `failed`, which is a fact about its own
action rather than a claim about the engine's index. A `failed` is the reason to distrust a
duplicated-looking result and call `ktsense_status`. `already_decided` says an earlier call in this
session settled the root and does not restate which way, which is a real limit of the signal.

Everything else in it is an index over the answer, not a second analysis. The commands each render one document per
question and are the authority on their own shape; re-deriving the same facts from a `--format json`
run would invoke the engine twice per call and let the text and the structure disagree. So the
citation index never asserts anything the text does not already say, and where the text carries no
positions the index carries none:

- `analyze_kotlin_dependencies` cites nothing. Its answer is a graph of package or file names with
  no lines in it, and at `--level file` the node names sit inside `a -> b` edge lines rather than
  being presented as files of their own.
- `get_kotlin_outline` and `get_kotlin_repo_map` cite files but no lines, because the skeletons they
  render carry no line numbers.
- `check_kotlin_syntax` is the only tool whose citations carry a column.
- A path is recognised only when it ends in `.kt` or `.kts` and contains no character a path cannot.
  The renderers neutralize source-derived text into visible `<U+XXXX>` markers first, so a file whose
  name carries a newline arrives as `ok.kt<U+000A>## Injected`, is not a path, and is left out rather
  than indexed as something it is not.
- The index stops at 500 citations and says how many it dropped.

An error result carries its citation index too. The files a failure names are as worth following as
the ones an answer names, and `check_kotlin_syntax` reports a file with syntax errors as an error
result whose text is the report.

## Review checklist

Ticked against the catalogue and the pinned snapshot at the commit that added this page.

- [x] Every tool states the question it answers, before any mention of how it works.
- [x] Every tool states when to prefer it over `grep`, over reading the file, or over another tool.
      `explain_kotlin_symbol` names the two tools to use instead of it while it is unimplemented.
- [x] Every tool carries a `requires:` marker matching its catalogue entry and a `cost:` marker that
      states a figure or admits none was measured, both asserted by
      `every_listed_tool_is_catalogued_read_only_and_states_its_requirement_and_its_cost`. The cost
      rule is itself tested: `cost: fast` is rejected, because that is the marker this page replaced.
- [x] Three requirement states are distinguished, so no description implies an engine-free path
      through `check_kotlin_syntax` or `find_kotlin_symbol`.
- [x] Every `cost:` figure traces to `.agents/scratchpad/ktsense/KT-38.md`, to the two measurements in
      `KT-32.md`, or to the `context` measurement in `KT-34.md`. Nothing is guessed, and a tool with no
      measurement is allowed to say `unmeasured` rather than being pushed into inventing a number.
- [x] No description calls a whole-tree tool fast. `deps` and `map` state their measured seconds.
- [x] `trace_kotlin_symbol` states that `index: partial` is a lower bound rather than the answer, and
      `find_kotlin_symbol` states that an unsettled index can duplicate one declaration into several
      candidates.
- [x] Accuracy honesty: resolution is stated as syntactic in the server instructions, and
      `analyze_kotlin_dependencies` states that its edges are imports rather than type-checked
      references.
- [x] Every tool in the catalogue answers for real. `explain_kotlin_symbol` was a KT-35 stub when this
      page was written and is not one now, so the `pending_card` field that recorded that state is
      gone rather than left as an always-empty option.
- [x] Version-check scope is stated where it is true, and not generalised.
- [x] Every tool is annotated `readOnlyHint: true` and `openWorldHint: false`; none of them writes.
- [x] Every tool declares an output schema, and all eight are identical.
- [x] No em dashes.
- [x] `tools/list` is pinned as a golden snapshot, so a description change is a reviewable diff.
- [x] Every tool is called over a real stdio session and its result pinned, including the ambiguity
      path of all three tools that resolve a name.

The snapshot is about a thousand lines, of which roughly four fifths is the output schema repeated
once per tool. That duplication is the protocol's, not the catalogue's: MCP gives each tool its own
`outputSchema` field and has no way to share one. A unit test asserts the eight are identical, so a
reviewer reads it once.

## Ambiguity is an answer, in all three tools that resolve a name

`find_kotlin_symbol`, `trace_kotlin_symbol` and `explain_kotlin_symbol` share one resolver, so they
share one ambiguity contract: a name that matches several declarations comes back as the candidate
list under exit 3, and `pick` with a fully-qualified name chooses one. Exit 3 rather than 2 because
clap owns 2 for a malformed invocation, and an agent has to be able to tell "I called this wrong",
which needs the call fixed, from "the name was ambiguous", which needs a candidate chosen.

The MCP layer treats exit 3 as an answer, not a failure: `isError` is false and the candidate list is
the content, because a list of candidates is what the question deserved. The three tools are pinned
together in `stdio_session__ambiguous_name.snap` so they cannot drift apart.

## Sessions pinned end to end

`crates/ktsense-mcp/tests/stdio_session.rs` drives real `ktsense mcp` processes over
newline-delimited JSON-RPC and pins `initialize`, `tools/list` and a `tools/call` for every tool the
server lists, as five golden snapshots. Each record carries the result's `isError`, its
`structuredContent` and its text, with absolute paths replaced by placeholders so the goldens do not
change with the checkout location.

The engine is the `fake_lsp` replay binary rather than a real `kmp-lsp`, so the default `cargo test`
needs no upstream install. Five sessions rather than one, because the fake is scripted by environment
and one script serves one conversation: `find` and `check` both read `FAKE_CMD_STDOUT` and want
different shapes in it, and a traced command's LSP session is a different conversation from a warm
client's.

| Snapshot | Covers |
|---|---|
| `no_index_tools` | `get_kotlin_outline` (three ways, including a missing file), `analyze_kotlin_dependencies` at both levels, `get_kotlin_repo_map`, `ktsense_status`, all while a warm engine is held |
| `symbol_lookup` | `find_kotlin_symbol` unique, limited, picked and unmatched |
| `trace_and_context` | `trace_kotlin_symbol`, and `explain_kotlin_symbol` at the default budget and at one small enough to drop items |
| `ambiguous_name` | the shared ambiguity contract across `trace_kotlin_symbol`, `explain_kotlin_symbol` and `find_kotlin_symbol` |
| `syntax_check` | `check_kotlin_syntax` over a directory with one broken file |

A sixth test asserts that the catalogue contains nothing the five sessions do not call, so a tool
added later fails the suite until it is covered rather than quietly going unexercised.

Two things a reader should know about the harness. It locates `ktsense` through `assert_cmd`, which
resolves it out of the target directory rather than from a `CARGO_BIN_EXE_` variable, because it is a
target of a different crate: `cargo test --workspace` builds every target first, so it is fresh, and a
package-scoped `cargo test -p ktsense-mcp` can leave a stale one behind. That is not hypothetical, it
happened while these tests were being written, and a stale binary made a passing snapshot record
stderr noise from a feature the binary predated. And the `trace_and_context` and `ambiguous_name`
sessions set `KTSENSE_MCP_NO_WARM_ENGINE=1`, because a warm client and a traced command drive
different conversations and one script cannot serve both; the warm path is proved by its own test and
by the measurements above instead.

## A known gap, described without a card id

`check_kotlin_syntax` maps a file with syntax errors onto `isError: true`, because the CLI exits 1 and
KT-31 mapped every non-answer exit to a tool error. That is defensible as "the tool's news is bad" but
arguable: MCP's `isError` means the tool failed to run, and a syntax check that found errors ran
perfectly. An agent branching on `isError` cannot tell "the engine is missing" from "your file has a
typo on line 12", and both are exit 1.

Changing it would change the KT-31 contract the exit-code mapping documents, so it is recorded here
rather than altered inside a card about descriptions. No card id is quoted on purpose: ids are the
board owner's to allocate, and an earlier draft of this page committed one that was already
provisionally in use for something else. The shape of the work, for whoever gets the id:

    ### KT-?? Let `check` report errors as an answer rather than a tool error
    - Phase: 3 | Size: S | Blocked by: KT-32
    - `ANSWER_EXITS` is `[0, 3]`, so `check` exiting 1 on a file with syntax errors becomes
      `isError: true` carrying the report. Either add a distinct exit for "checked, found errors" or
      let the MCP layer treat the check report as the answer it is.
    - Acceptance: a file with a syntax error returns `isError: false` with the report and its
      citations; a missing engine still returns `isError: true`; the CLI's own exit codes are
      unchanged, because a shell caller branches on them.
