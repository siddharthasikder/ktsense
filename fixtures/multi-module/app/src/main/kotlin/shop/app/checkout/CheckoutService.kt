package shop.app.checkout

import shop.app.reporting.AuditTrail
import shop.order.Order
import shop.order.OrderId
import shop.order.OrderRepository

class CheckoutService(
    private val repository: OrderRepository,
    private val auditTrail: AuditTrail,
) {
    fun placeOrder(order: Order): OrderId {
        val id = repository.save(order)
        auditTrail.record("placed ${order.customerEmail} as $id")
        return id
    }
}
