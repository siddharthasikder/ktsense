package app.service

import app.domain.Outcome
import app.domain.User
import app.domain.UserPredicate
import app.domain.UserRepository

/**
 * In-memory store used by tests and by the fixture's own main().
 *
 * Delegates the read side to a plain map wrapper so the parser sees `by` delegation.
 */
class InMemoryUserRepository private constructor(
    private val backing: MutableMap<Long, User>,
) : UserRepository, ReadOnlyView by MapView(backing) {

    constructor() : this(mutableMapOf())

    constructor(seed: Collection<User>) : this(seed.associateBy { it.id.raw }.toMutableMap())

    init {
        require(backing.size <= HARD_LIMIT) { "seed too large" }
    }

    override val size: Int get() = backing.size

    override fun findById(id: Long): User? = backing[id]

    override fun findAll(page: Int, pageSize: Int): List<User> =
        backing.values.drop(page * pageSize).take(pageSize)

    override fun findByEmail(email: String): User? = backing.values.firstOrNull { it.email == email }

    override fun findMatching(predicate: UserPredicate): List<User> = backing.values.filter(predicate)

    override suspend fun save(entity: User): Outcome<User> {
        backing[entity.id.raw] = entity
        return Outcome.Success(entity)
    }

    override suspend fun deleteById(id: Long): Boolean = backing.remove(id) != null

    override fun saveAll(entities: Collection<User>): Int {
        entities.forEach { backing[it.id.raw] = it }
        return entities.size
    }

    /** Nested: no reference to the outer instance. */
    class Stats(val reads: Long, val writes: Long)

    /** Inner: holds a reference to the outer instance, so `inner` matters to the parser. */
    inner class Snapshot {
        val takenFrom: Int = size
    }

    private companion object {
        const val HARD_LIMIT: Int = 10_000
    }
}

interface ReadOnlyView {
    fun keys(): Set<Long>
}

private class MapView(private val backing: Map<Long, User>) : ReadOnlyView {
    override fun keys(): Set<Long> = backing.keys
}
