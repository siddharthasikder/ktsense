package app.util

import app.domain.User

/** Wraps a page of results. Carries operator and infix members. */
class Page<T>(val items: List<T>, val index: Int) {

    operator fun get(position: Int): T = items[position]

    operator fun plus(other: Page<T>): Page<T> = Page(items + other.items, index)

    operator fun contains(item: T): Boolean = item in items

    infix fun mergedWith(other: Page<T>): Page<T> = this + other

    operator fun iterator(): Iterator<T> = items.iterator()
}

/** Extension property on a foreign type. */
val User.initials: String
    get() = (displayName ?: email).split(' ', '.').mapNotNull { it.firstOrNull() }.joinToString("")

/** Extension function with a vararg and a default. */
fun User.hasAnyEmailDomain(vararg domains: String, ignoreCase: Boolean = true): Boolean =
    domains.any { email.endsWith("@$it", ignoreCase) }

fun <T> pageOf(vararg items: T): Page<T> = Page(items.toList(), 0)
