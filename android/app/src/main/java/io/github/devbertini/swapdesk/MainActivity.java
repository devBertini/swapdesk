package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.content.pm.ApplicationInfo;
import android.os.Bundle;
import android.util.Log;
import android.widget.TextView;

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

        // Development builds only: "am start ... --es probe <address>" asks the core whether a Swapdesk pairing
        // endpoint answers there (the emulator reaches a development instance on its host at 10.0.2.2).
        boolean debuggable = (getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0;
        String probe = debuggable ? getIntent().getStringExtra("probe") : null;

        Thread check = new Thread(() -> {
            String report = probe == null ? Core.selfTest() : Core.probe(probe);
            for (String line : report.split("\n")) {
                Log.i(TAG, line);
            }
            runOnUiThread(() -> text.setText("Swapdesk " + version + "\n\n" + report));
        }, "swapdesk-selftest");
        check.start();
    }
}
