# Fixture: reference-kinds

A single-file Kotlin project whose only purpose is to pin KT-83: a `trace` must keep comment, KDoc
and string text, and same-named declarations, out of its callers. `kmp-lsp` 0.26.0 answers
`textDocument/references` with a whole-word text match, so a bare word search over this file reports
seven sites for `Sprocket`, of which only the code uses are callers.

It is a separate fixture rather than an addition to `multi-module` or `tiny-app` on purpose: both of
those are swept whole by golden tests (the repo map over `multi-module`, the outline and parse sweeps
over `tiny-app`), so a new file there would churn unrelated snapshots. This fixture is referenced only
by the KT-83 real-lsp trace test.

## Layout

One Gradle module, package `refkinds`, one source file.

| File | Why it exists |
|---|---|
| `src/main/kotlin/refkinds/Sprocket.kt` | Declares `class Sprocket` (the traced symbol) and mentions the word `Sprocket` in every way a caller answer must classify. |
| `settings.gradle.kts`, `build.gradle.kts` | Make it a real Kotlin JVM module so `kmp-lsp` resolves the source root. |

## The seven `Sprocket` sites

Tracing `refkinds.Sprocket` sees these whole-word matches:

| Where | Kind | In the answer? |
|---|---|---|
| `class Sprocket` (line 12) | the definition's own name | listed as the definition site |
| `return Sprocket()` (line 8) | code (constructor call) | a caller: `SprocketUser.assemble` |
| `fun assemble(): Sprocket` (line 6) | code (return type) | same caller, same declaration |
| `[Sprocket]` in the KDoc (line 3) | KDoc | left out, counted as a text mention |
| `Sprocket` in the line comment (line 5) | comment | left out, counted as a text mention |
| `Sprocket` in the string literal (line 7) | string | left out, counted as a text mention |
| `companion object Sprocket` (line 15) | another declaration's name | left out, counted as an other declaration |

## Expected `trace refkinds.Sprocket` answer

- **0 implementors:** `Sprocket` is a final class.
- **1 caller:** `refkinds.SprocketUser.assemble`, and no test callers.
- **Usages:** the code sites and the definition site, with the line
  `3 text mentions (comments, KDoc, strings), 1 other declaration named Sprocket` accounting for the
  four sites left out.
