package shop.app.reporting

class AuditTrail {
    private val entries = mutableListOf<String>()

    fun record(message: String) {
        entries.add(message)
    }

    fun all(): List<String> = entries.toList()
}
