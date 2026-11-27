package dev.coqo.sample;

import java.util.List;
import java.util.Map;

/**
 * A sample class.
 */
public class Sample {
    private static final int LIMIT = 10;
    private final String name;

    public Sample(String name) {
        this.name = name;
    }

    public String greet(List<String> others) {
        return "Hello " + name + others;
    }

    interface Listener {
        void onEvent(String event);
    }

    enum Mode {
        FAST,
        SLOW;

        boolean isFast() {
            return this == FAST;
        }
    }

    record Point(int x, int y) {}
}

@interface Marker {}
