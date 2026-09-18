package app

import app.domain.Role
import app.domain.User
import app.domain.UserId
import app.service.InMemoryUserRepository
import app.service.ServiceRegistry
import app.service.UserService
import app.util.SystemClock
import app.util.initials
import kotlinx.coroutines.runBlocking

fun main(args: Array<String>) {
    val repo = InMemoryUserRepository(seed = sampleUsers())
    val service = UserService(repo, SystemClock)
    ServiceRegistry.register(service)

    fun describe(user: User): String = "${user.email} (${user.initials})"

    runBlocking {
        service.findAll().forEach { println(describe(it)) }
        service.promote(UserId(1), Role.ADMIN)
    }

    println(ServiceRegistry.summary)
    println("args: ${args.joinToString()}")
}

private fun sampleUsers(): List<User> = listOf(
    User(UserId(1), "ada@example.com", "Ada Lovelace"),
    User(UserId(2), "grace@example.com", "Grace Hopper", setOf(Role.EDITOR)),
    User(UserId(3), "alan@example.com"),
)
