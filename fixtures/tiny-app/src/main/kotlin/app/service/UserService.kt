package app.service

import app.domain.Audited
import app.domain.Outcome
import app.domain.Role
import app.domain.Sanitized
import app.domain.User
import app.domain.UserId
import app.domain.UserRepository
import app.util.Clock
import app.util.slugify

/**
 * Application service for user lifecycle.
 *
 * The golden skeleton test in KT-09 renders this class, so keep its signatures stable.
 */
@Audited(category = "user")
open class UserService(
    private val repo: UserRepository,
    private val clock: Clock,
) : Service {

    override val name: String = "user-service"

    private val cache: MutableMap<UserId, User> = mutableMapOf()

    val cachedCount: Int
        get() = cache.size

    fun createUser(@Sanitized email: String, displayName: String? = null): Outcome<User> {
        if (email.isBlank()) {
            return Outcome.Failure("email is blank")
        }
        val user = User(
            id = UserId(clock.nowEpochMillis()),
            email = email,
            displayName = displayName ?: slugify(email),
        )
        cache[user.id] = user
        return Outcome.Success(user)
    }

    suspend fun findAll(page: Int = 0): List<User> = repo.findAll(page)

    suspend fun promote(id: UserId, role: Role): Outcome<User> {
        val existing = repo.findById(id.raw) ?: return Outcome.Failure("no such user: ${id.raw}")
        val promoted = existing.copy(roles = existing.roles + role)
        return repo.save(promoted)
    }

    internal fun evict(id: UserId): Boolean = cache.remove(id) != null

    protected open fun onEviction(user: User) {
        cache.remove(user.id)
    }

    private fun isCached(id: UserId): Boolean = cache.containsKey(id)

    companion object {
        const val MAX_PAGE_SIZE: Int = 100

        @JvmStatic
        fun describe(): String = "UserService(maxPage=$MAX_PAGE_SIZE)"
    }
}
