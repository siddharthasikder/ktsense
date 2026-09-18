package app.domain

/** A user of the system. Carries no behaviour beyond identity. */
data class User(
    val id: UserId,
    val email: String,
    val displayName: String? = null,
    val roles: Set<Role> = emptySet(),
)

@JvmInline
value class UserId(val raw: Long)

typealias UserPredicate = (User) -> Boolean

const val ANONYMOUS_DISPLAY_NAME: String = "anonymous"

val defaultPredicate: UserPredicate = { it.roles.isNotEmpty() }
