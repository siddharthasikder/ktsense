---
name: ktsense-kotlin
description: Answer questions about a Kotlin codebase with ktsense instead of reading files or grepping. Use when you need a file's API surface, where a declaration lives, who calls or implements it, the import graph, an orientation map of an unfamiliar repository, or a syntax check after an edit.
---

# ktsense: Kotlin code understanding

ktsense answers structural questions about a Kotlin workspace in compressed form, so you spend
context on the answer rather than on the file. Reach for it before `read` and before `grep`: an
outline gives you a file's signatures without its bodies, and "who implements this interface" is one
call rather than a grep and a pile of false positives.

Resolution is syntactic. ktsense parses with tree-sitter and asks an engine that indexes references;
it does not type-check. It cannot tell you which overload a call site resolves to, whether a type
argument is valid, or whether the code compiles. Type errors are Gradle's job. When ktsense is unsure
which declaration you meant it lists every candidate rather than guessing, and answers that depend on
the reference index carry a marker saying how complete that index was. Take both at face value.

## Picking a tool

| Your question | Tool | Cost |
|---|---|---|
| What is this file's API surface? | `get_kotlin_outline` | fast |
| Where is `Foo` declared? | `find_kotlin_symbol` | needs index |
| Who calls or implements `Foo`? | `trace_kotlin_symbol` | needs index |
| What depends on what? Are there cycles? | `analyze_kotlin_dependencies` | fast |
| I have never seen this repository | `get_kotlin_repo_map` | fast |
| Did my edit parse? | `check_kotlin_syntax` | fast |
| Give me everything about `Foo` in one bundle | `explain_kotlin_symbol` | needs index |
| Is the index complete yet? | `ktsense_status` | fast |

The markers say whether a tool waits for the reference index, not whether it needs the engine binary.
A tool marked fast answers without waiting for an index and stays cheap to call repeatedly. A tool
marked needs index starts or reuses an engine session, and the first such call on a cold repository is
the one that waits. The markers are in the tool descriptions too, so you can see the cost before you
call.

Three tools are pure tree-sitter and need no engine at all: `get_kotlin_outline`,
`analyze_kotlin_dependencies` and `get_kotlin_repo_map`. Every other tool reaches the engine,
`check_kotlin_syntax` included despite its fast marker, so on a host where `kmp-lsp` is missing those
fail whatever their marker says.

`get_kotlin_outline`, `analyze_kotlin_dependencies`, `get_kotlin_repo_map` and `trace_kotlin_symbol`
reuse a warm daemon session when one is running. Every other tool does its own work per call, so do
not assume that one warm call makes the next one warm.

Every tool takes an optional `root` naming the workspace to answer about. Leave it alone unless you
genuinely need to ask about a different checkout: the server was launched with a root and a wider one
answers about the wrong code rather than failing.

## Two habits worth keeping

**Run `check_kotlin_syntax` after every edit.** It does not wait for the index, and it catches the
broken brace or the stray paren immediately instead of at the next build. Point it at the file you
just touched, or at the directory if you touched several. Two things to know. It is a passthrough to
the engine's own checker, so it needs `kmp-lsp` installed and fails without it: if it errors on every
call rather than on the file you just changed, suspect a missing engine rather than your edit. And it
is syntax only, so a clean check does not mean the code compiles and is not a substitute for building.

**Call `ktsense_status` before a precision query you intend to rely on.** `trace_kotlin_symbol` and
`explain_kotlin_symbol` answer from the reference index, and on a cold or large repository the index
may not have finished. Rather than stall, `trace_kotlin_symbol` answers with what it has and marks
the answer `index: partial`. A partial answer is a real answer about the files indexed so far, but it
is not a complete list: treat "no callers" under `index: partial` as "none found yet", never as
"none exist". Under `index: complete` the list is the whole workspace. If you need completeness and
got `partial`, wait and ask again rather than reporting the weaker answer as though it were the
stronger one. Until `ktsense_status` itself ships (see the last section), the `index:` line on a
trace answer is the same signal after the fact rather than before it.

## Reading the results

An ambiguous name is an answer, not a failure. Ask `find_kotlin_symbol` for `save` in a repository
with three of them and you get all three, each with its fully qualified name, kind, file and line.
Choose one and pass its fully qualified name back as `pick`, on either `find_kotlin_symbol` or
`trace_kotlin_symbol`, to get the single declaration. Do not retry the bare name hoping for a
different outcome.

A genuine failure arrives as a tool error carrying ktsense's own message, so `no declaration named
ZzzNope` means the name is absent from the workspace rather than that something broke. Both outcomes
are actionable and neither needs a retry.

`trace_kotlin_symbol` reports callers as the declarations enclosing each reference site, because the
engine exposes no call hierarchy. That is a useful approximation of "who uses this" and it is not the
same thing as a call graph. `analyze_kotlin_dependencies` edges are import statements, not
type-checked references, so an unused import is still an edge.

Output is Markdown, shaped for reading rather than for parsing. If you need structured output, drive
the `ktsense` CLI directly with `--format json`.

## Working shapes

Landing in an unfamiliar repository, start with `get_kotlin_repo_map` for the shape of the thing,
then `analyze_kotlin_dependencies` to see which packages depend on which and whether there are
cycles. Both are fast, so neither costs you a wait.

Changing a declaration, find it with `find_kotlin_symbol`, learn the blast radius with
`trace_kotlin_symbol` (checking the index marker before you trust the caller list), make the edit,
and run `check_kotlin_syntax` on what you touched. Outline the file if you need the surrounding API
surface while editing, rather than reading the whole file.

Answering "is this safe to delete", `trace_kotlin_symbol` under `index: complete` is the evidence you
want. Under `index: partial` it is not, and saying so is better than being wrong.

## Not yet shipped

`explain_kotlin_symbol` and `ktsense_status` are catalogued and callable, and at the time of writing
each returns a tool error saying the underlying command is not implemented yet. Until they land, get
the same information the long way: `trace_kotlin_symbol` plus `get_kotlin_outline` in place of
`explain_kotlin_symbol`, and the `index:` marker on a trace answer in place of `ktsense_status`. If a
call returns `is not implemented yet`, that is this gap and not a misconfiguration.
