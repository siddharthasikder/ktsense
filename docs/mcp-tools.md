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
| `explain_kotlin_symbol` | `context` | kmp-lsp and a settled index | unmeasured | not implemented, KT-35 |
| `ktsense_status` | `status` | nothing | ~100 ms, 1990 files | KT-32 |

Every KT-38 figure is a median of nine timed repetitions on ktor 3.0.1 from
`.agents/scratchpad/ktsense/KT-38.md`, in-process rather than through a daemon. The two KT-32 figures
were taken the same way, from a release binary, with the exit status checked so a failing command
cannot be reported as a latency. No figure appears in a description that is not in one of those two
files.

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

## `structuredContent`

Every result carries the command's Markdown as its text content and an `Answer` object as its
`structuredContent`, declared by an output schema identical across all eight tools:

| Field | Meaning |
|---|---|
| `tool`, `command` | Which tool answered and the `ktsense` subcommand it delegated to. |
| `requires` | The same three-state marker as the description. |
| `root` | The workspace root the answer is about, which the cited paths are relative to. |
| `exit` | The command's exit status: `0` an answer, `3` an ambiguous name, `70` not implemented. |
| `index` | `complete` or `partial`, when the answer carried an index marker. |
| `files` | Files the answer is about, in the order it introduced them. |
| `citations` | Every `path`, `line` and optional `column` the answer cites. |
| `citations_omitted` | How many further citations the text has and this index does not. |

It is an index over the answer, not a second analysis. The commands each render one document per
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
- [x] Every tool carries a `requires:` marker matching its catalogue entry, asserted by
      `every_listed_tool_is_catalogued_read_only_and_states_its_requirement_and_pending_card`.
- [x] Three requirement states are distinguished, so no description implies an engine-free path
      through `check_kotlin_syntax` or `find_kotlin_symbol`.
- [x] Every `cost:` figure traces to `.agents/scratchpad/ktsense/KT-38.md` or to the two
      measurements recorded in `.agents/scratchpad/ktsense/KT-32.md`; the one tool with no
      measurement says `unmeasured` rather than guessing, and says why.
- [x] No description calls a whole-tree tool fast. `deps` and `map` state their measured seconds.
- [x] `trace_kotlin_symbol` states that `index: partial` is a lower bound rather than the answer.
- [x] Accuracy honesty: resolution is stated as syntactic in the server instructions, and
      `analyze_kotlin_dependencies` states that its edges are imports rather than type-checked
      references.
- [x] `explain_kotlin_symbol` says it is not implemented and names KT-35, held in the catalogue as
      `pending_card` so the description cannot drift from the fact.
- [x] Version-check scope is stated where it is true, and not generalised.
- [x] Every tool is annotated `readOnlyHint: true` and `openWorldHint: false`; none of them writes.
- [x] Every tool declares an output schema, and all eight are identical.
- [x] No em dashes.
- [x] `tools/list` is pinned as a golden snapshot, so a description change is a reviewable diff.

The snapshot is about a thousand lines, of which roughly four fifths is the output schema repeated
once per tool. That duplication is the protocol's, not the catalogue's: MCP gives each tool its own
`outputSchema` field and has no way to share one. A unit test asserts the eight are identical, so a
reviewer reads it once.

## Known gap and a proposed card

`check_kotlin_syntax` maps a file with syntax errors onto `isError: true`, because the CLI exits 1
and KT-31 mapped every non-answer exit to a tool error. That is defensible as "the tool's news is
bad" but arguable: MCP's `isError` means the tool failed to run, and a syntax check that found
errors ran perfectly. Changing it would change the KT-31 contract the exit-code mapping documents,
so it is recorded here rather than altered inside a card about descriptions.

    ### KT-57 Let `check` report errors as an answer rather than a tool error
    - Phase: 3 | Size: S | Blocked by: KT-32
    - `ANSWER_EXITS` is `[0, 3]`, so `check` exiting 1 on a file with syntax errors becomes
      `isError: true` carrying the report. An agent that branches on `isError` cannot tell "the
      engine is missing" from "your file has a typo on line 12", and both are exit 1. Either add a
      distinct exit for "checked, found errors" or let the MCP layer treat the check report as the
      answer it is.
    - Acceptance: a file with a syntax error returns `isError: false` with the report and its
      citations; a missing engine still returns `isError: true`; the CLI's own exit codes are
      unchanged, because a shell caller branches on them.
