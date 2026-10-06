package app;

public class StagingToggle {
    public StagingConfig enable(StagingConfig config) {
        config.setStagingEnabled(true);
        return config;
    }
}
