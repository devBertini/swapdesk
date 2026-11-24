package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.util.Log;
import android.widget.TextView;

import java.net.Inet4Address;

/**
 * The development harness, in the debug sources only (the release sources hold a class of this name that does
 * nothing, and the release library has none of these native methods). A script on the development PC starts the
 * app with extras and reads the system log:
 *
 * <pre>
 * am start -n io.github.devbertini.swapdesk/.MainActivity
 *     --es dev "ttl=2"                 development options of the core (the emulator needs TTL 2)
 *     --es selftest 1                  the checks of slice A1: keys, a pinned-key QUIC handshake, Opus
 *     --es probe ADDRESS [--es probe_mode local|net|local+net]
 *                                      whether a pairing endpoint answers there
 *     --es harness "viewer ADDRESS"    the scripted viewer: add by address, pair, connect, forget
 * </pre>
 */
final class DevHarness {
    private static final String TAG = "Swapdesk";

    private DevHarness() {}

    private static native String selfTest();

    private static native String probe(String spec);

    private static native String run(String spec);

    /** The development options the intent asks of the core, or none. */
    static String options(Activity activity) {
        String dev = activity.getIntent().getStringExtra("dev");
        return dev == null ? "" : dev;
    }

    /** Runs what the intent asks for, off the main thread, and logs each line of its report. */
    static void run(Activity activity, TextView status) {
        String probe = probeSpec(activity);
        String script = activity.getIntent().getStringExtra("harness");
        boolean selfTest = activity.getIntent().getStringExtra("selftest") != null;
        if (probe == null && script == null && !selfTest) {
            return;
        }
        Thread worker = new Thread(() -> {
            String report = selfTest ? selfTest() : probe != null ? probe(probe) : run(script);
            for (String line : report.split("\n")) {
                Log.i(TAG, line);
            }
            activity.runOnUiThread(() -> status.append("\n\n" + report));
        }, "swapdesk-harness");
        worker.start();
    }

    /** The probe the intent asks for, with the address and the handle of the Wi-Fi network when its mode names them. */
    private static String probeSpec(Activity activity) {
        String address = activity.getIntent().getStringExtra("probe");
        String mode = activity.getIntent().getStringExtra("probe_mode");
        if (address == null || mode == null) {
            return address;
        }
        ConnectivityManager networks = activity.getSystemService(ConnectivityManager.class);
        for (Network network : allNetworks(networks)) {
            NetworkCapabilities caps = networks.getNetworkCapabilities(network);
            LinkProperties link = networks.getLinkProperties(network);
            if (caps == null || link == null || !caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                    || caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                continue;
            }
            for (LinkAddress a : link.getLinkAddresses()) {
                if (a.getAddress() instanceof Inet4Address) {
                    String spec = address;
                    if (mode.contains("local")) {
                        spec += " local=" + a.getAddress().getHostAddress();
                    }
                    if (mode.contains("net")) {
                        spec += " net=" + network.getNetworkHandle();
                    }
                    if (mode.contains("ttl2")) {
                        spec += " ttl=2";
                    }
                    return spec;
                }
            }
        }
        Log.i(TAG, "PROBE FAIL: no Wi-Fi network with an IPv4 address");
        return address;
    }

    @SuppressWarnings("deprecation") // the harness reads the networks once; the app itself listens with a callback
    private static Network[] allNetworks(ConnectivityManager networks) {
        return networks.getAllNetworks();
    }
}
