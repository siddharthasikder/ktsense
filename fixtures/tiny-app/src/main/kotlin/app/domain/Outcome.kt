package app.domain

/**
 * Outcome of a domain operation.
 *
 * Sealed so exhaustive `when` works without an else branch.
 */
sealed interface Outcome<out T> {
    data class Success<out T>(val value: T) : Outcome<T>

    data class Failure(val reason: String, val cause: Throwable? = null) : Outcome<Nothing>

    object Pending : Outcome<Nothing>

    val isTerminal: Boolean
        get() = this !is Pending
}

sealed class ValidationError(val field: String) {
    class Blank(field: String) : ValidationError(field)
    class TooLong(field: String, val limit: Int) : ValidationError(field)
    class Malformed(field: String, val pattern: String) : ValidationError(field)
}

inline fun <T, R> Outcome<T>.map(transform: (T) -> R): Outcome<R> = when (this) {
    is Outcome.Success -> Outcome.Success(transform(value))
    is Outcome.Failure -> this
    Outcome.Pending -> Outcome.Pending
}
