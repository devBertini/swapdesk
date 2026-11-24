package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.widget.TextView;

/**
 * A released app has no development harness: this class stands in for the one of the debug sources and does
 * nothing, and the release library has none of its native methods.
 */
final class DevHarness {
    private DevHarness() {}

    static String options(Activity activity) {
        return "";
    }

    static void run(Activity activity, TextView status) {}
}
