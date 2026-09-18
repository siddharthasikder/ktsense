package app.util

/** SAM conversion target, so the parser must record `fun interface`. */
fun interface Clock {
    fun nowEpochMillis(): Long
}

object SystemClock : Clock {
    override fun nowEpochMillis(): Long = System.currentTimeMillis()
}

class FixedClock(private val fixed: Long) : Clock {
    override fun nowEpochMillis(): Long = fixed
}

fun slugify(input: String): String =
    input.lowercase().replace(NON_SLUG, "-").trim('-')

fun truncate(input: String, limit: Int = 40): String =
    if (input.length <= limit) input else input.take(limit - 1) + "\u2026"

private val NON_SLUG = Regex("[^a-z0-9]+")
