package app;

public class UpdateByName extends UpdateBase {
    public String run(String name) {
        return executeUpdate(name);
    }

    @Override
    public UpdateGuard guard() {
        return null;
    }
}
