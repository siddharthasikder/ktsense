package shop.db

import shop.order.Order
import shop.order.OrderId
import shop.order.OrderRepository

class JdbcOrderRepository(private val url: String = "jdbc:h2:mem:orders") : OrderRepository {
    override fun save(order: Order): OrderId = order.id

    override fun findById(id: OrderId): Order? = null
}
