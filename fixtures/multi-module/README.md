# Fixture: multi-module

A real three-module Gradle Kotlin project whose only purpose is to give the cross-file features a
codebase with a known-good answer: a symbol traced across modules, an import graph with a documented
external edge, and one deliberately planted package-level import cycle. It is never published and
never linked into ktsense itself.

9 Kotlin files, 5 Gradle Kotlin scripts. `kmp-lsp check` reports all 14 files OK and `kmp-lsp
sources` resolves all three module source roots, so any ERROR node or missing root a later test finds
is that test's fault, not the fixture's.

## Module and package layout

Three Gradle modules wired through `settings.gradle.kts`, with a conventional acyclic module
dependency direction: `app -> db -> core`, and `app -> core`.

| Gradle module | Package | Files | Role |
|---|---|---|---|
| `core` | `shop.order` | `OrderRepository.kt`, `Order.kt` | Declares the `OrderRepository` interface (the traced symbol) and the `Order` / `OrderId` value types. Depends on nothing internal. |
| `db` | `shop.db` | `JdbcOrderRepository.kt`, `InMemoryOrderRepository.kt` | The two implementations of `OrderRepository`. Depends only on `core`. |
| `app` | `shop.app.checkout` | `CheckoutService.kt`, `OrderImporter.kt`, `CheckoutConfig.kt` | Callers of `save`, plus one half of the planted cycle. |
| `app` | `shop.app.reporting` | `AuditTrail.kt`, `ReportBackfill.kt` | A caller of `save`, plus the other half of the planted cycle. |

There are **4 internal packages**: `shop.order`, `shop.db`, `shop.app.checkout`, `shop.app.reporting`.

## What each file is for

| File | Why it exists |
|---|---|
| `core/.../shop/order/OrderRepository.kt` | The single definition of the traced symbol `save`, plus `findById`. This is the one interface declaration a `trace` must report as the definition. |
| `core/.../shop/order/Order.kt` | `data class Order` and `data class OrderId`, the value types every module shares. |
| `db/.../shop/db/JdbcOrderRepository.kt` | Implementor 1 of `OrderRepository`. Its `save` is an override, not a call. |
| `db/.../shop/db/InMemoryOrderRepository.kt` | Implementor 2 of `OrderRepository`. Its `save` is an override, not a call. Carries two of the three external imports (`java.util.concurrent.*`). |
| `app/.../shop/app/checkout/CheckoutService.kt` | Caller 1 of `save`. Imports `shop.app.reporting.AuditTrail`, which is the `checkout -> reporting` cycle edge. |
| `app/.../shop/app/checkout/OrderImporter.kt` | Caller 2 of `save`. Imports `shop.db.InMemoryOrderRepository`, giving the `checkout -> db` edge and justifying the `app -> db` module dependency. |
| `app/.../shop/app/checkout/CheckoutConfig.kt` | An `object` of `const val` config. Imported by `reporting`, which is the `reporting -> checkout` cycle edge. |
| `app/.../shop/app/reporting/AuditTrail.kt` | The reporting type imported by `checkout`; the target of the `checkout -> reporting` edge. |
| `app/.../shop/app/reporting/ReportBackfill.kt` | Caller 3 of `save`. Imports `shop.app.checkout.CheckoutConfig` (the `reporting -> checkout` cycle edge) and the third external import (`java.time.Instant`). |
| `settings.gradle.kts` | Wires the three modules (`include("core", "db", "app")`) so this is one real multi-module build. |
| `build.gradle.kts` (root), `*/build.gradle.kts` | Declare the Kotlin JVM plugin and each module's project dependencies so `kmp-lsp sources` resolves every `src/main/kotlin` root. |

## Expected answers

These are frozen. A later card should be able to assert against them without re-deriving anything.

### Expected `trace` answer (KT-18)

Tracing the symbol `shop.order.OrderRepository.save` yields exactly:

- **1 definition:** `fun save(order: Order): OrderId` in `core/src/main/kotlin/shop/order/OrderRepository.kt`.
- **2 implementors:** `JdbcOrderRepository.save` and `InMemoryOrderRepository.save`, both in module `db`.
- **3 callers** (the enclosing declaration of each `repository.save(...)` call site):
  1. `shop.app.checkout.CheckoutService.placeOrder`
  2. `shop.app.checkout.OrderImporter.importAll`
  3. `shop.app.reporting.ReportBackfill.rebuild`

There is no other declaration named `save` and no other `save(` call site anywhere in the fixture,
so these counts are exact.

### Expected `deps` answer (KT-20)

The package import graph has 4 nodes and these directed edges (deduplicated across files):

| From package | To package | Resolves to |
|---|---|---|
| `shop.db` | `shop.order` | workspace (module `core`) |
| `shop.app.checkout` | `shop.order` | workspace (module `core`) |
| `shop.app.checkout` | `shop.db` | workspace (module `db`) |
| `shop.app.checkout` | `shop.app.reporting` | workspace (module `app`) |
| `shop.app.reporting` | `shop.order` | workspace (module `core`) |
| `shop.app.reporting` | `shop.app.checkout` | workspace (module `app`) |

**External imports** (unresolved in the workspace, so marked external). There are exactly 3, all in
the `db` and `reporting` packages:

- `java.util.concurrent.ConcurrentHashMap` (in `shop.db`)
- `java.util.concurrent.atomic.AtomicLong` (in `shop.db`)
- `java.time.Instant` (in `shop.app.reporting`)

Every `shop.*` import resolves to a workspace file. No `kotlin.*` imports appear, because the stdlib
symbols used are auto-imported.

### Expected cycle answer (KT-20)

Exactly one cycle. Tarjan's algorithm reports a single non-trivial strongly-connected component:

    { shop.app.checkout, shop.app.reporting }

The two edges that close it are `shop.app.checkout -> shop.app.reporting` (via
`CheckoutService` importing `AuditTrail`) and `shop.app.reporting -> shop.app.checkout` (via
`ReportBackfill` importing `CheckoutConfig`).

**Why this is a package cycle and not a module cycle:** both packages live inside the single `app`
Gradle module, so no module imports itself and the module graph `app -> db -> core` stays acyclic and
buildable. A cycle between Gradle modules would not compile; a cycle between packages inside one
module is legal Kotlin, which is precisely the case this fixture pins down.

## Invariants

Later cards depend on these, so changing them breaks tests rather than just shifting a snapshot.

- `OrderRepository.save` is the KT-18 trace target. Its 1 definition / 2 implementors / 3 callers
  shape is frozen; add members or callers elsewhere rather than editing these.
- The only planted cycle is `shop.app.checkout` <-> `shop.app.reporting`. Do not add an import that
  would create a second cycle or break this one.
- The three external imports listed above are the only imports that do not resolve inside the
  fixture. Adding another external import changes the KT-20 external-edge count.
- Module dependency direction stays `app -> db -> core` (plus `app -> core`) and acyclic.
- Note: the executable plan's Phase-2 gate line guessed "3 packages"; the fixture deliberately has 4
  (a 2-package cycle in `app` plus separate `core` and `db` packages). This README is the
  authoritative count.

## Verification

    RUST_LOG=error kmp-lsp check .            # All 14 files OK
    RUST_LOG=error kmp-lsp sources --root .   # all three module source roots resolve
