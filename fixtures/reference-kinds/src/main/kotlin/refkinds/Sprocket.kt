package refkinds

/** A gadget wired to a [Sprocket]; this KDoc link documents the type, it does not call it. */
class SprocketUser {
    // A Sprocket named in a line comment is prose, not a caller.
    fun assemble(): Sprocket {
        val note = "assembling a Sprocket from a string literal is not a call"
        return Sprocket()
    }
}

class Sprocket

class Registry {
    companion object Sprocket
}
