package app

/** A hand-written declaration, the control against the generated one beside it. */
class Widget(val id: Int) {
    fun render(): String = "widget-$id"
}
