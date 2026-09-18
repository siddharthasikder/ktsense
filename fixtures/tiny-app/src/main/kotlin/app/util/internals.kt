package app.util

/**
 * Everything here is private or internal. `outline` without `--private` should render an empty
 * skeleton for this file, which is the case that catches an over-eager renderer.
 */
private const val SALT: String = "ktsense"

private class Hasher(private val salt: String = SALT) {
    fun hash(input: String): Int = (input + salt).hashCode()
}

internal fun internalHash(input: String): Int = Hasher().hash(input)

private fun unusedHelper(): Nothing = throw UnsupportedOperationException()
