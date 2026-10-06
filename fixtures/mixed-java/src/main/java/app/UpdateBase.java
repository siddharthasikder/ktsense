package app;

/** Base class whose protected update method is overridden and called across Java and Kotlin. */
public abstract class UpdateBase {
    protected String executeUpdate(String item) {
        return "updated " + item;
    }

    // executeUpdate is called by subclasses; this comment names it without using it.
    public abstract UpdateGuard guard();
}
