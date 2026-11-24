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
     * Runs the core's checks on this device (keys, a pinned-key QUIC handshake, Opus) and returns one line per
     * check after a first line "SELFTEST passed/all". It opens sockets: never call it on the main thread.
     */
    static native String selfTest();

    /**
     * Development harness: whether a Swapdesk pairing endpoint answers from this device. {@code spec} is the
     * address ("10.0.2.2:47851"), then optionally " local=IPv4" (the production socket: on that address of this
     * device, TTL 1) and " net=handle" (bound to that network, {@code Network.getNetworkHandle()}). One line:
     * "PROBE ANSWERED", "PROBE REFUSED", "PROBE TIMEOUT" or "PROBE FAIL", each with its reason. Never on the main
     * thread.
     */
    static native String probe(String spec);
}
