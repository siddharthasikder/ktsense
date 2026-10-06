package app

/** Stands in for a KSP-generated wrapper the engine does not index, under build/generated/ksp. */
class GeneratedWidget(val widget: Widget) {
    fun handle(): String = widget.render()
}
