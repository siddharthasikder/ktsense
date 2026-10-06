package app

class UpdateByDomain : UpdateBase() {
    fun run(domain: String): String {
        return executeUpdate(domain)
    }

    override fun guard(): UpdateGuard? = null
}
