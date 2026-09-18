package app.service

import app.domain.Repository
import app.domain.User

/** Singleton registry. `object` at top level, with a `by lazy` property. */
object ServiceRegistry {
    private val services: MutableList<Service> = mutableListOf()

    val summary: String by lazy { services.joinToString(", ") { it.name } }

    fun register(service: Service) {
        services += service
    }

    fun <S : Service> find(type: Class<S>): S? = services.filterIsInstance(type).firstOrNull()

    fun startAll() = services.forEach(Service::start)
}

/** Reified type parameter, so the parser must keep `reified` on the type parameter list. */
inline fun <reified T : Service> ServiceRegistry.findOrNull(): T? = find(T::class.java)

fun countUsers(repo: Repository<User>): Int = repo.size
