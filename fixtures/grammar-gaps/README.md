# Fixture: grammar-gaps

Minimal reproductions of the four Kotlin constructs `brokk-tree-sitter-kotlin` 0.4.6 cannot parse.
Isolated under KT-52 from the five files `ktsense outline` rejects in the pinned benchmark corpora
(kotlinx.coroutines 1.9.0 and ktor 3.0.1).

This directory is **not** a workspace and **not** a Gradle project. It is a set of one-construct
files, each paired with a control that differs by a single token, and the `.gap.kt` half is
*expected* to produce an ERROR node. `crates/ktsense-syntax/tests/grammar_gaps.rs` asserts exactly
that, so when upstream fixes the grammar the test fails and names the construct that started
parsing. Do not point `bench/compress.sh` or a corpus sweep at this directory expecting a clean run.

Every construct here is valid Kotlin: each one is live code in a release-tag file of a project whose
CI compiles it. `kmp-lsp check` 0.26.0 reports an error on each `.gap.kt` file at the same position
and nothing on any `.ok.kt` file, which is expected because upstream binds the same grammar. That
agreement is what places the defect upstream rather than in this crate.

## The four constructs

| Pair | Construct the grammar rejects | Control differs by | Corpus files |
|---|---|---|---|
| `nullable-callable-reference` | a nullable receiver before `::` (`String?::plus`) | drops the `?` | `ZipTest.kt`, `CombineTest.kt` |
| `semicolon-opening-class-body` | a lone `;` as the first token inside a class body (`class Holder {;`) | drops the `;` | `CancelledParentAttachTest.kt` |
| `parenthesized-expression-statement` | a parenthesized expression standing as a statement (`(action)`) | drops the parentheses | `FormAuth.kt` |
| `declaration-then-callable-reference` | a local declaration followed by a statement that starts with `::` | drops the `::` | `CoroutineScopeTest.kt` |

Both halves of the last pair are load-bearing: `::local` alone is fine, and a local declaration
followed by any other statement is fine. Only the pair fails. An explicit `;` after the declaration
clears it, which is what identifies it as a statement-boundary defect rather than invalid source.

The nullable-receiver gap is about the `?` and nothing else. The receiver of `::` is parsed as an
expression (`navigation_expression` over `navigation_suffix`), and an expression cannot carry a
nullable-type marker, so `String?` is not representable in that position. Writing the gap with a
space, `String? ::plus`, shows this directly: the `navigation_suffix ::plus` parses, and the `?`
alone lands in an ERROR node as a `quest`. Written without the space, `?:` mis-lexes toward elvis and
the tree becomes an `elvis_expression` with the second colon reported instead, which is why the
committed fixture reports a column that looks like it blames the `::`. A qualified receiver
(`foo.Bar::baz`) and a generic one (`List<String>::size`) both parse cleanly, so nullability is the
sole discriminator.

The parenthesized-expression gap is about statement position, not about the call. `(action)()` in an
initializer parses; `(action)` on its own as a statement does not. The corpus instance is
`(authenticationFunction)(call, it)` inside a lambda body, so the call suffix is incidental.

Evidence, per-construct grammar diagnosis and the drafted upstream report are in the KT-52 notes.
Reproduce with `cargo run -p ktsense-syntax --example error_nodes -- fixtures/grammar-gaps/*.kt`.
