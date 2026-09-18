package shop.app.checkout

import shop.db.InMemoryOrderRepository
import shop.order.Order
import shop.order.OrderRepository

class OrderImporter(private val repository: OrderRepository = InMemoryOrderRepository()) {
    fun importAll(orders: List<Order>): Int {
        var count = 0
        for (order in orders) {
            repository.save(order)
            count++
        }
        return count
    }
}
