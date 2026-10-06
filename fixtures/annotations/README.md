# annotations fixture (KT-109)

A minimal workspace for the annotation use-site answer. Kept separate from `multi-module` so the
map, deps and outline goldens over that fixture do not move; this follows the `generated-sources`
precedent of a dedicated feature fixture driven by the fake engine.

| File | Exercises |
|---|---|
| `src/main/kotlin/app/Audited.kt` | declares `annotation class Audited` |
| `src/main/kotlin/app/Reports.kt` | `@Audited class SalesReport`; `@Audited @RequestRouter class AuditReport` |
| `src/main/kotlin/app/handlers/Tasks.kt` | `@Audited fun runTask()` in a second package |

Expected answers:

- `trace Audited` resolves to the annotation class and lists `## Annotated (3)`: `app.SalesReport`
  and `app.AuditReport` under `Reports.kt`, `app.handlers.runTask` under `handlers/Tasks.kt`.
- `trace RequestRouter` finds no declaration and lists `## Annotated (1)` (`app.AuditReport`) above
  its `## Text references`, exit 1.
