package shop.app.reporting

import java.time.Instant
import shop.app.checkout.CheckoutConfig
import shop.order.Order
import shop.order.OrderRepository

class ReportBackfill(private val repository: OrderRepository) {
    fun rebuild(orders: List<Order>): Int {
        var restored = 0
        for (order in orders) {
            if (restored >= CheckoutConfig.maxItems) break
            repository.save(order)
            restored++
        }
        return restored
    }

    fun stamp(): Instant = Instant.now()
}
