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

    static boolean secure(Activity activity, boolean real) {
        return real;
    }

    static void run(Activity activity, TextView status) {}

    static PaintView screen(Activity activity) {
        return null;
    }

    static boolean scripted(Activity activity) {
        return false;
    }

    static void watch(Activity activity, PaintView view) {}
}
