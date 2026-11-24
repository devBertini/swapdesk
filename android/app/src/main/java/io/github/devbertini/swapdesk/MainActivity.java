package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.content.pm.ApplicationInfo;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.os.Bundle;
import android.util.Log;
import android.widget.TextView;

import java.net.Inet4Address;

/** Slice A1: the app starts, loads the core and says what the core's checks found on this device. */
public final class MainActivity extends Activity {
    private static final String TAG = "Swapdesk";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        TextView text = new TextView(this);
        int pad = Math.round(24 * getResources().getDisplayMetrics().density);
        text.setPadding(pad, pad, pad, pad);
        setContentView(text);

        String version = Core.version();
        Log.i(TAG, "core " + version);
        text.setText("Swapdesk " + version);

        // Development builds only: "am start ... --es probe <address> [--es probe_mode local|net|local+net]" asks
        // the core whether a Swapdesk pairing endpoint answers there (the emulator reaches a development instance on
        // its host at 10.0.2.2), from any socket or from the production one on the Wi-Fi network.
        boolean debuggable = (getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0;
        String probe = debuggable ? probeSpec() : null;

        Thread check = new Thread(() -> {
            String report = probe == null ? Core.selfTest() : Core.probe(probe);
            for (String line : report.split("\n")) {
                Log.i(TAG, line);
            }
            runOnUiThread(() -> text.setText("Swapdesk " + version + "\n\n" + report));
        }, "swapdesk-selftest");
        check.start();
    }

    /** The probe the intent asks for, with the Wi-Fi network's address and handle when its mode names them. */
    private String probeSpec() {
        String address = getIntent().getStringExtra("probe");
        if (address == null) {
            return null;
        }
        String mode = getIntent().getStringExtra("probe_mode");
        if (mode == null) {
            return address;
        }
        ConnectivityManager networks = getSystemService(ConnectivityManager.class);
        for (Network network : allNetworks(networks)) {
            NetworkCapabilities caps = networks.getNetworkCapabilities(network);
            LinkProperties link = networks.getLinkProperties(network);
            if (caps == null || link == null) {
                continue;
            }
            Log.i(TAG, "NETWORK " + link.getInterfaceName() + " wifi=" + caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                    + " cellular=" + caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)
                    + " vpn=" + caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
                    + " addresses=" + link.getLinkAddresses()
                    + " default=" + network.equals(networks.getActiveNetwork()));
            if (!caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) || caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
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
