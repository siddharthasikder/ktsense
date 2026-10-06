package app;

import lombok.Data;

/** A Lombok-style bean: @Data generates setStagingEnabled, so no setter is declared in source. */
@Data
public class StagingConfig {
    private boolean stagingEnabled;
}
