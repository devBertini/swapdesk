package io.github.devbertini.swapdesk;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.net.Inet4Address;
import java.net.NetworkInterface;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The networks of the phone for the guard of the core (docs/ANDROID.md 4): it listens to every network Android
 * tells apps of and, at each change, hands the core the whole list. Which of them Swapdesk may use is decided in
 * the core (Wi-Fi or Ethernet that is not a VPN); this class only reports.
 */
final class Networks extends ConnectivityManager.NetworkCallback {
    private static final String TAG = "Swapdesk";
    // The bits of crates/swapdesk-sys-android/src/facts.rs.
    private static final int WIFI = 1;
    private static final int ETHERNET = 2;
    private static final int CELLULAR = 4;
    private static final int VPN = 8;
    private static final int OTHER = 16;
    private static final int MAX_NETWORKS = 16;
    private static final int MAX_ADDRESSES = 8;
    private static final int MAX_NAME = 32;

    private static Networks instance;

    private final Map<Network, NetworkCapabilities> capabilities = new LinkedHashMap<>();
    private final Map<Network, LinkProperties> links = new LinkedHashMap<>();

    private Networks() {}

    /** Starts listening, once for the process of the app. */
    static synchronized void start(Context context) {
        if (instance != null) {
            return;
        }
        ConnectivityManager manager = context.getApplicationContext().getSystemService(ConnectivityManager.class);
        instance = new Networks();
        // Every network, whatever it can do: the core must see a VPN and mobile data to refuse them.
        manager.registerNetworkCallback(new NetworkRequest.Builder().clearCapabilities().build(), instance);
        instance.report();
    }

    @Override
    public synchronized void onCapabilitiesChanged(Network network, NetworkCapabilities caps) {
        capabilities.put(network, caps);
        report();
    }

    @Override
    public synchronized void onLinkPropertiesChanged(Network network, LinkProperties link) {
        links.put(network, link);
        report();
    }

    @Override
    public synchronized void onLost(Network network) {
        capabilities.remove(network);
        links.remove(network);
        report();
    }

    /** Hands the core the list as it is now. */
    private synchronized void report() {
        String approved = Core.networks(encode());
        Log.i(TAG, "NETWORKS " + approved);
    }

    private byte[] encode() {
        List<byte[]> records = new ArrayList<>();
        for (Map.Entry<Network, LinkProperties> entry : links.entrySet()) {
            NetworkCapabilities caps = capabilities.get(entry.getKey());
            LinkProperties link = entry.getValue();
            String name = link.getInterfaceName();
            if (caps == null || name == null || records.size() == MAX_NETWORKS) {
                continue;
            }
            byte[] nameBytes = name.getBytes(StandardCharsets.UTF_8);
            if (nameBytes.length > MAX_NAME) {
                continue;
            }
            List<LinkAddress> addresses = new ArrayList<>();
            for (LinkAddress a : link.getLinkAddresses()) {
                if (a.getAddress() instanceof Inet4Address && addresses.size() < MAX_ADDRESSES) {
                    addresses.add(a);
                }
            }
            ByteBuffer b = ByteBuffer.allocate(8 + 4 + 1 + 1 + nameBytes.length + 1 + addresses.size() * 5)
                    .order(ByteOrder.LITTLE_ENDIAN);
            b.putLong(entry.getKey().getNetworkHandle());
            b.putInt(index(name));
            b.put((byte) transports(caps));
            b.put((byte) nameBytes.length);
            b.put(nameBytes);
            b.put((byte) addresses.size());
            for (LinkAddress a : addresses) {
                b.put(a.getAddress().getAddress());
                b.put((byte) a.getPrefixLength());
            }
            records.add(b.array());
        }
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        out.write(records.size());
        for (byte[] record : records) {
            out.write(record, 0, record.length);
        }
        return out.toByteArray();
    }

    private static int transports(NetworkCapabilities caps) {
        int bits = 0;
        if (caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
            bits |= WIFI;
        }
        if (caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)) {
            bits |= ETHERNET;
        }
        if (caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)) {
            bits |= CELLULAR;
        }
        if (caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
            bits |= VPN;
        }
        // Anything this app does not name is "other", which the guard never approves.
        return bits == 0 ? OTHER : bits;
    }

    private static int index(String name) {
        try {
            NetworkInterface nic = NetworkInterface.getByName(name);
            return nic == null ? 0 : nic.getIndex();
        } catch (Exception e) {
            return 0;
        }
    }
}
