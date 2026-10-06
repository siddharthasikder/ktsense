# Fixture: mixed-java

A workspace with both Java and Kotlin sources, for the KT-112 behaviour where `trace` and `context`
report where Java sources hold references the Kotlin engine cannot resolve.

| File | Role |
|---|---|
| `src/main/java/app/UpdateBase.java` | Java base class; declares the protected method `executeUpdate` and references the Kotlin interface `UpdateGuard` |
| `src/main/java/app/UpdateById.java` | Java caller of `executeUpdate`; references `UpdateGuard` |
| `src/main/java/app/UpdateByName.java` | Java caller of `executeUpdate`; references `UpdateGuard` |
| `src/main/kotlin/app/UpdateByDomain.kt` | Kotlin subclass that calls the Java `executeUpdate` and uses `UpdateGuard` |
| `src/main/kotlin/app/UpdateGuard.kt` | Kotlin interface declared here and referenced from Java |

Expected answers:

- `trace executeUpdate` resolves to a `.java` definition (`UpdateBase.java`), so the engine resolves
  no Kotlin references and reports `Callers (0 from Kotlin)`. The Java text references list the
  declaration and its two Java callers; the Kotlin text references list the call in
  `UpdateByDomain.kt` attributed to `app.UpdateByDomain.run`.
- `trace UpdateGuard` resolves to a `.kt` definition (`UpdateGuard.kt`), so the engine resolves its
  Kotlin callers normally while the Java text references list the three `.java` files that name it.
  No Kotlin text references section is added and no Java-definition note is printed.
