# Agent evaluation: ktsense against a grep-equipped baseline

Ten questions about `kotlinx.coroutines` at commit `d8d6f8f`, each asked of two Kiro CLI agents that
differ in exactly one respect. The baseline has `read`, `grep`, `glob`, `ls` and `shell`. The second
arm has those five plus the eight tools of the ktsense MCP server. So the question this answers is
what ktsense adds to an agent that can already search a repository, not what it adds to an agent with
no tools at all.

Run on 2026-09-21 with `bench/agent-eval/run.sh`, ktsense built from `349f300`, engine `kmp-lsp
0.26.0`, `kiro-cli 2.22.1`. Twenty-six sessions in total: twenty for the matrix at one session per
cell, and six more on one question to bound the timing claim.

## The result

Both arms answered all nine graded questions correctly. ktsense was nominally slower on nine of the ten
cells, by a margin this host's noise does not resolve.

| Question | Kind | baseline | ktsense | ktsense tools it chose |
|---|---|---|---|---|
| Q01 | locate declaration | correct, 8.5 s, 1 call | correct, 10.5 s, 1 call | `find_kotlin_symbol` |
| Q02 | locate implementor | correct, 21.3 s, 3 calls | correct, 23.6 s, 3 calls | `trace_kotlin_symbol`, then two greps |
| Q03 | supertypes | correct, 10.8 s, 1 call | correct, 12.0 s, 1 call | `explain_kotlin_symbol` |
| Q04 | member type | correct, 12.6 s, 2 calls | correct, 14.6 s, 2 calls | `find_kotlin_symbol`, then a read |
| Q05 | locate declaration | correct, 13.1 s, 2 calls | correct, 20.2 s, 3 calls | `find_kotlin_symbol`, then grep and read |
| Q06 | overloads | ungraded, 49.0 s, 3 calls | ungraded, 42.1 s, 3 calls | none: grep and read only |
| Q07 | multiplatform expect/actual | correct, 11.0 s, 1 call | correct, 12.3 s, 1 call | none: one grep |
| Q08 | module dependencies | correct, 15.6 s, 2 calls | correct, 21.3 s, 2 calls | none: two greps |
| Q09 | two hop | correct, 10.4 s, 1 call | correct, 19.0 s, 4 calls | `find_kotlin_symbol` twice, `trace_kotlin_symbol` |
| Q10 | locate internal declaration | correct, 8.9 s, 1 call | correct, 12.1 s, 1 call | `find_kotlin_symbol` |

| Arm | Sessions | Graded | Correct | Wrong | No answer line | Ungraded | Median wall | Tool calls | of which ktsense |
|---|---|---|---|---|---|---|---|---|---|
| baseline | 10 | 9 | 9 | 0 | 0 | 1 | 11.0 s | 17 | 0 |
| ktsense | 10 | 9 | 9 | 0 | 0 | 1 | 14.6 s | 21 | 9 |

Every number in both tables comes from one session per cell. Nine correct out of nine, twice over, is
a tie at the resolution this sample can see, and it would look the same whether ktsense helps a little
or not at all.

## Reading the result

The honest summary is that ktsense did not win. It matched the baseline on correctness, used more tool
calls in total, 21 against 17, and was nominally about a third slower at the median, though that gap is
inside this host's noise and is not a measured cost: see the null control below.

The reason is visible in the questions rather than in the tools. Nine of the ten ask where a named
declaration lives or what it declares, and `kotlinx.coroutines` gives almost every declaration a
distinctive name. A single `grep` for `public interface CoroutineScope` lands on the answer with no
false positives, so the baseline finished Q01 in one call and 8.5 seconds. There is nothing for a
structural tool to improve on when a text search is already exact. Q09 was written to defeat that by
requiring two hops with the second target unnamed, and the baseline defeated the question instead: it
grepped for `MutexImpl`, read the supertype off the declaration line, and grepped again for the name
it found, in one or three calls depending on the run.

What ktsense did buy, on the seven questions where the arm chose to use it, was fewer bytes read per
answer. Q03 is the clearest case: one `explain_kotlin_symbol` call returned the declaration site and
the signature `open class JobSupport(active: Boolean) : Job, ChildJob, ParentJob` in 1.8 seconds of
tool time, and the session answered without opening a file. The baseline reached the same answer by
grepping and reading the matched line. Both are one call; only one of them involved a compressed
answer rather than a raw line of source. This evaluation does not measure context consumed, so that
difference is an observation from the transcripts and not a result.

On three of ten questions the ktsense-equipped arm ignored ktsense entirely and reached for `grep`.
Those are Q06, Q07 and Q08: counting overloads, enumerating `expect` and `actual` declarations, and
finding a module's dominant third-party import. Two of the three are questions the tool surface
arguably covers, since `find_kotlin_symbol` enumerates declarations and `analyze_kotlin_dependencies`
reports import edges. The agent was told nothing about when to prefer which tool, and no skill file was
installed. That is a finding about tool discoverability rather than about tool quality, and it is the
one result here that points at product work.

## Proof that the with-MCP arm really had the tools

A server declared in a `settings/mcp.json` file reaches a session only when the active agent sets
`"useLegacyMcpJson": true`. Without it the session starts with none of the tools and is
indistinguishable from a session that had them and declined to use them, which would turn this whole
document into a comparison of the baseline against itself. `run.sh` therefore declares the server
inside the agent configuration, where it is always honoured, and proves the tools arrived three ways.

A direct JSON-RPC handshake against `ktsense --root <corpus> mcp`, with no model involved, listed
eight tools. A preflight session was then told to call `get_kotlin_repo_map`, and Kiro reported the
call as `from mcp server: ktsense`. Both gates are hard failures: the run aborts rather than starting
the matrix. Beyond that, every cell records the number of ktsense calls it made, and a ktsense-arm
session that makes zero is still scored and is additionally flagged as a failed cell, which makes the
run exit non-zero. It is not withheld from the table, so the credit needs stating plainly: 2 of the 9
correct answers on the ktsense arm, Q07 and Q08, were reached without calling ktsense at all. Nine
calls were observed across seven of the ten cells, and the per-call arguments are in the transcripts
under `runs/full-1/transcripts/`.

An earlier version of the preflight gated on the agent listing all eight tool names, and it failed
twice against a correctly configured server: asked to name its tools, the model omitted the one it was
about to call. Self-report is not evidence of configuration, so the gate now rests on the handshake and
the observed call, and the echoed name count is recorded for interest only.

## The bin duplication, and a correction

The corpus holds 1025 `.kt` files that ktsense's traversal walks, out of 1592 on disk: 214 are the
byte-identical `bin` copies left by the Eclipse Buildship import that `kmp-lsp` triggers on a Gradle
project, and 353 more sit under `build`, `target` and friends. So `find` and `grep -r` see all 1592,
while `rg` lands at 1239 because it honours the corpus `.gitignore`, which excludes `build` and `out`
but not `bin`. An agent reaching for any of them can answer a counting question wrongly through no fault
of its own.

**Both arms were told to ignore `bin`, in identical words, in every prompt.** The preamble in
`questions.yaml` names `bin`, `build`, `target`, `.gradle` and `node_modules` as out of scope, and it
is prepended verbatim to each question for each arm. Ground truth was established with the same
exclusions. Nothing was deleted from the corpus.

The assumption this card was briefed with, that ktsense sees the true 1025 files and only the baseline
sees the duplicates, is false as stated, and pruning would have been the wrong fix. ktsense excludes
`bin` in its own tree-sitter traversal, which covers `get_kotlin_outline`,
`analyze_kotlin_dependencies` and `get_kotlin_repo_map`. The engine-backed tools do not: the engine
maintains its own index and indexes `bin` with everything else. Run directly against this corpus,
`trace CoroutineScope --pick kotlinx.coroutines.CoroutineScope` reports 77 implementors, and 137 lines
of that output cite a path under `bin/`. So the exclusion has to be asked for in the prompt, which both
arms get equally, rather than assumed from either arm's tooling. Deleting the directories would not
have held either, since the import that wrote them runs again the next time the engine opens the
project.

This did not change any graded answer here, and that was checked rather than assumed. Eight of the nine
graded questions resolve to files under `kotlinx-coroutines-core`, which carries no `bin` copies at all.
The ninth, Q08, targets `reactive/kotlinx-coroutines-rx2`, which does: 35 duplicated `.kt` files. Their
effect on that answer is nil, because counting the duplicates in raises `io.reactivex` from 58 imports
to 116 and `org.junit` from 35 to 70, leaving the ranking the question asks about unchanged. No graded
question asks for a repository wide count, which is where the duplication would have bitten.

## Wall time is indicative, not a benchmark

Between four and six other agents were compiling on this 32 core host throughout. The one minute load
average was sampled next to every cell and ranged from 4.48 to 11.92 across the twenty matrix cells; the
quieter 2.07 belongs only to the Q09 repetitions, which ran later. Available memory never fell
below 17.8 GB, against a 4 GB floor at which `run.sh` stops. Timing is taken with the bash `time`
keyword, the method `bench/latency.sh` uses, so nothing forks inside the measured span.

Two things bound what these numbers can mean. Each matrix cell is one sample, and the span is dominated
by model latency rather than tool latency, so it reports how many turns an arm needed more than how
fast anything ran.

Q06, Q07 and Q08 are an accidental null control, because on those three the ktsense arm used no ktsense
tool and so both arms ran identical tooling: the ktsense-arm span came out between 7.0 s faster and
5.7 s slower than the baseline. A 12.7 s noise band around zero swallows the 3.6 s median difference
reported above, so that difference is unresolved at this sample size rather than a measured cost.

The one claim with a real spread behind it is Q09, repeated to four sessions per arm:

| Arm | n | Samples (ms) | Median |
|---|---|---|---|
| baseline | 4 | 10394, 11526, 13200, 19486 | 12.4 s |
| ktsense | 4 | 18133, 19040, 26133, 28777 | 22.6 s |

The ranges touch at the edges, so ktsense being slower on this question is probably real rather than
noise, but four samples per arm on one of ten questions does not support a claim about ktsense's
latency in general. Correctness on Q09 was 4 of 4 for both arms.

## Ground truth, and the question that lost its

Every answer was established by reading the corpus, with the command recorded in `established_by`
beside each question. No answer came from a ktsense tool or from an agent session, because an answer
produced by one of the arms cannot then grade that arm.

Q06 is ungraded, and the reason is worth stating plainly because it is the harness's weakest link.
Asked how many `buffer` extension declarations on `Flow` the repository contains, this file originally
said two. Both arms said four and named two declarations in a test file that the ground truth had
missed. They were right. The original figure came from searching for `fun <T> Flow<T>.buffer`, a
pattern that requires the type parameter and so cannot match the `Flow<Int>.buffer` forms declared in
`FlowInvariantsTest.kt`. Re-checked with a pattern that does not presuppose it, there are four real
declarations plus one textual match inside a KDoc block.

The row stays ungraded rather than rescored. The wording "count only real declarations" does not settle
whether a function declared inside a test function body is one, so no single number is defensible; and
the grading patterns as written would have scored both arms correct by coincidence, because each
mentioned "2 in Context.kt" on the way to saying four. A row that passes for the wrong reason is not
evidence, whichever way it falls. The corrected ground truth is recorded in `questions.yaml` for
whoever rewrites the question.

Grading is mechanical: each question carries regular expressions that must match the arm's final
`ANSWER:` line, and `grade.py regrade` rescores a finished run from its stored transcripts, so a
correction to a question costs no credits and no rerun.

## What this does not show

The question set does not test what ktsense is mainly for. There is no question here about a budgeted
repository map, a ranked outline of an unfamiliar module, a dependency graph, or a symbol whose name is
ambiguous enough that a text search returns a hundred candidates and the agent has to choose. Those are
the cases where a compressed structural answer should beat a raw match list, and they are also the
cases where ground truth is hardest to establish, which is why this set avoided them. A set built to
discriminate would need harder ground truth work than this card had room for, and it is proposed as a
follow-up rather than smuggled in here.

Nothing here measures context window consumption, which is ktsense's stated reason for existing.
Counting bytes returned per answer would be a better metric than wall time and is not something these
transcripts record cleanly.

## Reproducing

```
cargo build --release -p ktsense-cli
bench/agent-eval/run.sh --root bench/repos/kotlinx.coroutines
```

Twenty sessions, run strictly one at a time. The run recorded here spent 349 seconds inside sessions and
cost 11.45 credits including the preflight. `--only Q09 --reps 3` reproduces the repetition table.
`run.sh` writes its own throwaway Kiro workspace under `$TMPDIR` and deletes it afterwards; it never
reads or writes `~/.kiro`, which on a shared host belongs to every other live session. Results land in
`bench/agent-eval/runs/<timestamp>/`, which is not committed.
