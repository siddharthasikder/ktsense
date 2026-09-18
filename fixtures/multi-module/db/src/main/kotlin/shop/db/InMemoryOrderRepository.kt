package shop.db

import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong
import shop.order.Order
import shop.order.OrderId
import shop.order.OrderRepository

class InMemoryOrderRepository : OrderRepository {
    private val store = ConcurrentHashMap<Long, Order>()
    private val sequence = AtomicLong(0)

    override fun save(order: Order): OrderId {
        val id = OrderId(sequence.incrementAndGet())
        store[id.value] = order.copy(id = id)
        return id
    }

    override fun findById(id: OrderId): Order? = store[id.value]
}
