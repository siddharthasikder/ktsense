package app.service

/** Marker for anything with a lifecycle the container manages. */
interface Service {
    val name: String

    fun start() {}

    fun stop() {}
}

abstract class AbstractCachingService<K, V : Any>(
    protected val capacity: Int,
) : Service {

    private val entries: LinkedHashMap<K, V> = LinkedHashMap(capacity)

    abstract fun load(key: K): V?

    abstract val hitRatio: Double

    fun get(key: K): V? = entries[key] ?: load(key)?.also { entries[key] = it }

    final override fun stop() {
        entries.clear()
    }
}
