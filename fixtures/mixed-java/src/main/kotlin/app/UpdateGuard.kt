package app

interface UpdateGuard {
    fun validate(input: String): Boolean
}
