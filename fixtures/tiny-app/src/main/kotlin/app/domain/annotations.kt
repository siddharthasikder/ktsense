package app.domain

@Target(AnnotationTarget.CLASS, AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.RUNTIME)
annotation class Audited(val category: String = "default")

@Target(AnnotationTarget.VALUE_PARAMETER)
annotation class Sanitized

@Repeatable
@Target(AnnotationTarget.FUNCTION)
annotation class RetryOn(val exception: String)
