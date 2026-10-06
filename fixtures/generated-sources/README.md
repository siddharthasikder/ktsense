# fixture: generated-sources

A minimal tree with one hand-written source and one generated source, for the KT-104 behaviour that a
declaration lookup reads generated Kotlin under `build/generated` even though `build` is otherwise
ignored.

| File | Declaration | Role |
|---|---|---|
| `src/main/kotlin/app/Widget.kt` | `app.Widget` | hand-written control |
| `build/generated/ksp/main/kotlin/app/GeneratedWidget.kt` | `app.GeneratedWidget` | generated, read only by a declaration lookup and labelled `generated` |

Expected answers:
- `symbols GeneratedWidget` (engine finds nothing) falls back to the syntax index, lists
  `app.GeneratedWidget` with `source: syntax index` and a `(generated)` marker.
- `symbols --contains Widget` lists both, the generated one marked `generated`.
- `map` and `outline` ignore `build`, so neither surfaces `GeneratedWidget`.

The generated file is committed on purpose; `git check-ignore` must report it as not ignored so the
fixture travels with the repository.
