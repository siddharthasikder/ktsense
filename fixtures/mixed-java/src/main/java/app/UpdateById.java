package app;

public class UpdateById extends UpdateBase {
    public String run(String id) {
        return executeUpdate(id);
    }

    @Override
    public UpdateGuard guard() {
        return null;
    }
}
