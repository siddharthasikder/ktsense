package app.service

import app.domain.Outcome
import app.domain.User
import app.domain.ValidationError

/**
 * Deliberately awkward signatures. These exist to catch compressor bugs: a signature that must
 * wrap, a `where` clause, nested generics, and a function type as a return type.
 */
class Validator<T> where T : Any {

    fun validate(candidate: T, rules: List<(T) -> ValidationError?>): List<ValidationError> =
        rules.mapNotNull { rule -> rule(candidate) }

    fun combine(
        first: (T) -> ValidationError?,
        second: (T) -> ValidationError?,
    ): (T) -> ValidationError? = { candidate -> first(candidate) ?: second(candidate) }

    fun validateAllReturningVeryLongTypeName(
        candidates: Collection<T>,
        stopOnFirstFailure: Boolean = false,
    ): Map<T, List<ValidationError>> = candidates.associateWith { emptyList() }

    fun <R : Comparable<R>> sortedBy(candidates: List<T>, selector: (T) -> R?): List<T> =
        candidates.sortedWith(compareBy(selector))
}

fun userValidator(): Validator<User> = Validator()

fun requireSuccess(outcome: Outcome<User>): User = when (outcome) {
    is Outcome.Success -> outcome.value
    is Outcome.Failure -> error(outcome.reason)
    Outcome.Pending -> error("still pending")
}
