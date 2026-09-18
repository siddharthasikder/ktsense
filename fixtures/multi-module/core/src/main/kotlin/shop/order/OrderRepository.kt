package shop.order

interface OrderRepository {
    fun save(order: Order): OrderId

    fun findById(id: OrderId): Order?
}
