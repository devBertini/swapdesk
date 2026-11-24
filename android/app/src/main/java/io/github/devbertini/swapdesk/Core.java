package io.github.devbertini.swapdesk;

/** The Rust core (crates/swapdesk-app-android), loaded once with the class. */
final class Core {
    static {
        System.loadLibrary("swapdesk_android");
    }

    private Core() {}

    /** The app's version and the protocol it speaks, as the core says them. */
    static native String version();

    /**
     * Starts the service core once (a second call starts nothing): the device key, the trust store and the logs
     * live under {@code dir}, the app's private folder, sealed with {@code dataKey} (32 bytes, {@link DataKey}).
     * {@code dev} is empty outside a development build. Returns what happened; a refusal starts with "ERROR ".
     */
    static native String start(String dir, byte[] dataKey, String name, String osVersion, String dev);

    /**
     * The phone's connected networks, whole, at every change ({@link Networks} writes the buffer). Returns what
     * the guard approves now; a refusal starts with "ERROR ".
     */
    static native String networks(byte[] facts);
}
