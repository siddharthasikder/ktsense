package app.util

import app.domain.Outcome

/** Retry helper: suspend function taking a suspend lambda, so the parser sees both. */
suspend fun <T> retrying(
    attempts: Int = 3,
    onFailure: (Throwable, Int) -> Unit = { _, _ -> },
    block: suspend () -> T,
): Outcome<T> {
    var lastError: Throwable? = null
    repeat(attempts) { attempt ->
        try {
            return Outcome.Success(block())
        } catch (error: IllegalStateException) {
            lastError = error
            onFailure(error, attempt)
        }
    }
    return Outcome.Failure("exhausted $attempts attempts", lastError)
}

suspend fun <T, R> Iterable<T>.mapSequentially(transform: suspend (T) -> R): List<R> {
    val out = ArrayList<R>()
    for (item in this) {
        out += transform(item)
    }
    return out
}

fun buildLabel(prefix: String, build: StringBuilder.() -> Unit): String =
    StringBuilder(prefix).apply(build).toString()
