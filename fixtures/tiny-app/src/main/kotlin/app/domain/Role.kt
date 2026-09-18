package app.domain

/** Access levels, ordered from least to most privileged. */
enum class Role(val weight: Int) {
    VIEWER(0) {
        override fun canMutate(): Boolean = false
    },
    EDITOR(10) {
        override fun canMutate(): Boolean = true
    },
    ADMIN(100) {
        override fun canMutate(): Boolean = true
    };

    abstract fun canMutate(): Boolean

    companion object {
        fun heaviest(roles: Collection<Role>): Role = roles.maxByOrNull(Role::weight) ?: VIEWER
    }
}
