package app.legacy;

import java.util.List;
import java.util.Optional;

/**
 * Pre-Kotlin audit log. Present so the fixture exercises the Java path: kmp-lsp indexes Java too,
 * and ktsense must not claim a Java file is Kotlin.
 */
public interface AuditLog {

    void record(String actor, String action);

    Optional<String> lastAction(String actor);

    List<String> history(String actor, int limit);

    default boolean isEmpty() {
        return history("", 1).isEmpty();
    }

    final class Entry {
        private final String actor;
        private final String action;
        private final long timestamp;

        public Entry(String actor, String action, long timestamp) {
            this.actor = actor;
            this.action = action;
            this.timestamp = timestamp;
        }

        public String actor() {
            return actor;
        }

        public String action() {
            return action;
        }

        public long timestamp() {
            return timestamp;
        }
    }
}
