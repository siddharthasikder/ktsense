# Fixture: tiny-app

A single-module Kotlin project whose only purpose is to make parser and compressor bugs visible.
Every file exists to cover declaration kinds that `ktsense-syntax` must extract and that the
Markdown compressor must render. It is never published and never linked into ktsense itself.

15 Kotlin files, 1 Java file, 2 Gradle Kotlin scripts. `kmp-lsp check` reports all 18 files OK, so
any ERROR node a parser test finds is the parser's fault, not the fixture's.

## What each file is for

| File | Constructs it covers |
|---|---|
| `app/Main.kt` | `main` entry point, local function inside a function body, lambda bodies, private top-level function, an unresolvable external import (`kotlinx.coroutines`) for the deps card |
| `app/domain/User.kt` | `data class` with defaults and a nullable property, `@JvmInline value class`, `typealias` of a function type, top-level `const val` and `val` |
| `app/domain/Role.kt` | `enum class` with a constructor parameter, per-entry class bodies overriding a member, `abstract fun` in an enum, `companion object` with a method reference |
| `app/domain/Outcome.kt` | `sealed interface` with generic variance, nested `data class` and `object` variants, `Nothing` type argument, property with a getter only, `sealed class` hierarchy, `inline fun` extension with an exhaustive `when` |
| `app/domain/Repository.kt` | `interface` with covariant `out T`, default method body, `suspend` members, default parameter values, interface inheritance chain three levels deep |
| `app/domain/annotations.kt` | `annotation class` with `@Target` and `@Retention`, one with a default argument, one `@Repeatable` |
| `app/service/Service.kt` | plain `interface` with empty default bodies, `abstract class` with two type parameters and a bound, `protected val` constructor property, `abstract val`, `final override` |
| `app/service/UserService.kt` | the golden-skeleton target: primary constructor with `private val` properties, supertype, `override val`, property with a custom getter, `@Sanitized` parameter annotation, `suspend` methods, `internal` and `protected open` and `private` members, `companion object` with `const val` and `@JvmStatic` |
| `app/service/InMemoryUserRepository.kt` | `private constructor`, two secondary constructors, `init` block, interface delegation with `by`, `override` of every inherited member, `class` nested vs `inner class`, `private companion object`, a second top-level `private class` in the same file |
| `app/service/ServiceRegistry.kt` | top-level `object` singleton, `by lazy` delegated property, generic method with a bound, method reference in a lambda, `inline fun` with a `reified` type parameter |
| `app/service/Validator.kt` | `where` clause on a class, function types as parameters and as a return type, nested generics in a return type, a deliberately long signature that must wrap, generic method with `Comparable` bound |
| `app/util/Page.kt` | `operator fun` for `get`, `plus`, `contains` and `iterator`, `infix fun`, extension property with a getter on a foreign type, extension function with `vararg` plus a defaulted parameter |
| `app/util/Clock.kt` | `fun interface` for SAM conversion, `object` implementing it, `class` implementing it, expression-body top-level functions, private top-level `Regex` property, a unicode escape in a string literal |
| `app/util/Retry.kt` | `suspend` generic function taking a `suspend` lambda, defaulted lambda parameter, `try`/`catch`, `repeat` with an index, extension on `Iterable` with a suspend transform, function with a receiver-typed lambda (`StringBuilder.() -> Unit`) |
| `app/util/internals.kt` | a file whose declarations are all `private` or `internal`; `outline` without `--private` must render this as empty, which catches an over-eager renderer |
| `app/legacy/AuditLog.java` | Java interop: `public interface`, `default` method, `Optional` and `List` return types, a `final` nested class with a constructor and accessor methods |
| `build.gradle.kts`, `settings.gradle.kts` | make the directory a real Gradle project so `kmp-lsp sources` resolves `src/main/kotlin` and `src/main/java` as source roots |

## Invariants

Later cards depend on these, so changing them breaks tests rather than just shifting a snapshot.

- `UserService` is the class the KT-09 golden skeleton renders. Its signatures are frozen; add
  members rather than editing existing ones.
- `app/util/internals.kt` must stay free of any public declaration.
- The `kotlinx.coroutines` import in `Main.kt` is the only import that does not resolve inside the
  fixture. KT-20 counts it as the one external edge.
- Every construct in the table above appears at least once. If a parser test needs a kind that is
  missing, add it here and add a row, rather than inventing a snippet inside the test.

## Verification

    RUST_LOG=error kmp-lsp sources --root .   # both source roots resolve
    RUST_LOG=error kmp-lsp check .            # All 18 files OK
