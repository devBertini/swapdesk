package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.content.pm.ActivityInfo;
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
 *     --es mock SCENARIO               a state of the design's mock on the shared screens, as the desktops' --mock
 *         [--es fit 1]                 the desktop window of that state, whole, as large as the screen allows
 *         [--es landscape 1]           with the screen held sideways
 *     --es add ADDRESS[:PORT]          on the app's own screens: adds the computer at that address (the port is
 *                                      a development instance's; the screens' address field takes none)
 *         [--es frames N]              N frames painted in a row, each logged with what it cost (default 1)
 *                                      and, after every frame that changed it, a STATE line: the pane, what is
 *                                      over it, and where each control is (for taps sent with adb)
 * </pre>
 */
final class DevHarness {
    private static final String TAG = "Swapdesk";

    private DevHarness() {}

    private static native String selfTest();

    private static native String probe(String spec);

    private static native String run(String spec);

    private static native String mock(String scenario);

    private static native String state();

    private static native void add(String address);

    /** The development options the intent asks of the core, or none. */
    static String options(Activity activity) {
        String dev = activity.getIntent().getStringExtra("dev");
        return dev == null ? "" : dev;
    }

    /** Whether the intent asks for a run that has no screen of its own: the self-test, a probe, a scripted viewer. */
    static boolean scripted(Activity activity) {
        return activity.getIntent().getStringExtra("selftest") != null
                || activity.getIntent().getStringExtra("probe") != null
                || activity.getIntent().getStringExtra("harness") != null;
    }

    /**
     * The screen the intent asks for in place of the app's own: a state of the design's mock, with the mock's own
     * backend behind it, so taps, lists and fields answer as they do behind the desktops' --mock. Null when none.
     */
    static PaintView screen(Activity activity) {
        String scenario = activity.getIntent().getStringExtra("mock");
        if (scenario == null) {
            return null;
        }
        if (activity.getIntent().getStringExtra("landscape") != null) {
            activity.setRequestedOrientation(ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE);
        }
        String said = mock(scenario);
        Log.i(TAG, "MOCK " + scenario + " " + said);
        if (!said.startsWith("OK ")) {
            return null;
        }
        PaintView view = new PaintView(activity);
        view.keepScenario();
        if (activity.getIntent().getStringExtra("fit") != null) {
            String[] size = said.split(" ");
            view.fit(Float.parseFloat(size[1]), Float.parseFloat(size[2]));
        }
        String frames = activity.getIntent().getStringExtra("frames");
        view.trace(frames == null ? 1 : Integer.parseInt(frames));
        watch(activity, view);
        return view;
    }

    /**
     * For a script that drives a screen by real touches (adb): after every frame that changed it, a STATE line says
     * where the screen is and where its controls are, in the screen's own pixels. With the extra {@code add}, the
     * computer at that address (which may carry a port) is added once the screen is up.
     */
    static void watch(Activity activity, PaintView view) {
        String[] last = {""};
        view.afterFrame(() -> {
            String now = state();
            if (!now.equals(last[0])) {
                last[0] = now;
                int[] origin = new int[2];
                view.getLocationOnScreen(origin);
                Log.i(TAG, "STATE origin=" + origin[0] + "," + origin[1] + " " + now);
            }
        });
        String address = activity.getIntent().getStringExtra("add");
        if (address != null) {
            view.postDelayed(() -> {
                Log.i(TAG, "ADD " + address);
                add(address);
                view.requestFrame();
            }, 1500);
        }
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
