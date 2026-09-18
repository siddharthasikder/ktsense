package app.domain

/** Read side of a store. Covariant so a `Repository<User>` is a `Repository<Any>`. */
interface Repository<out T> {
    val size: Int

    fun findById(id: Long): T?

    fun findAll(page: Int = 0, pageSize: Int = 20): List<T>

    /** Default implementation so most stores need not repeat it. */
    fun isEmpty(): Boolean = size == 0
}

/** Write side, kept separate so read-only callers cannot mutate. */
interface MutableRepository<T> : Repository<T> {
    suspend fun save(entity: T): Outcome<T>

    suspend fun deleteById(id: Long): Boolean

    fun saveAll(entities: Collection<T>): Int
}

interface UserRepository : MutableRepository<User> {
    fun findByEmail(email: String): User?

    fun findMatching(predicate: UserPredicate): List<User>
}
