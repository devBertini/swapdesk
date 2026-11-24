package io.github.devbertini.swapdesk;

/**
 * The shared screens, painted by the Rust core (crates/swapdesk-app-android, {@code phone_screen}): the same ones
 * Windows and the Mac show. Every method is called from the main thread, where the core keeps the screen.
 */
final class Shell {
    static {
        System.loadLibrary("swapdesk_android");
    }

    private Shell() {}

    /** The interface's face, Instrument Sans (a variable font), which the core carries for every system. */
    static native byte[] font();

    /** Tells the core the ascent and descent of the phone's monospaced face, as fractions of its size. */
    static native void monoMetrics(float ascent, float descent);

    /**
     * One pass of a frame for a view of this size in device pixels at {@code density} device pixels per DIP.
     * The buffer starts with 0, a frame for {@link PaintView} to replay, or with 1, a list of text for
     * {@link TextMeasurer}, whose answer goes to {@link #measured} before this is called again.
     */
    static native byte[] paint(int widthPx, int heightPx, float density);

    /** Hands the measures of the last list to the core. Empty when taken; a refusal starts with "ERROR ". */
    static native String measured(byte[] answer);

    /** What the last frame cost, for the trace: {@code lists=N labels=N forced=0|1}. */
    static native String cost();
}
