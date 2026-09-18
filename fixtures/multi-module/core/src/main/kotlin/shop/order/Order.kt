package shop.order

data class OrderId(val value: Long)

data class Order(
    val id: OrderId,
    val customerEmail: String,
    val totalCents: Long,
)
