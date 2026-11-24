package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.TextView;

/**
 * The app before its shared screens (slice A3): it starts the core, tells it the networks of the phone, and asks
 * for the local network where Android makes that a permission, saying what stops working when it is refused.
 */
public final class MainActivity extends Activity {
    private static final String TAG = "Swapdesk";
    /** Android 17: from here the local network is a permission the person grants (docs/ANDROID.md 4). */
    private static final int LOCAL_NETWORK_FROM = 37;
    private static final String LOCAL_NETWORK = "android.permission.ACCESS_LOCAL_NETWORK";
    private static final int ASK_LOCAL_NETWORK = 1;
    private static final String ASKED_LOCAL_NETWORK = "local_network_asked";

    private TextView status;
    private TextView notice;
    private Button openSettings;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        int pad = Math.round(24 * getResources().getDisplayMetrics().density);
        LinearLayout column = new LinearLayout(this);
        column.setOrientation(LinearLayout.VERTICAL);
        column.setPadding(pad, pad, pad, pad);
        // Android draws an app behind the bars of the system: a frame keeps clear of them (it takes the bars as its
        // own padding), and the column keeps its margins inside it.
        FrameLayout frame = new FrameLayout(this);
        frame.setFitsSystemWindows(true);
        frame.addView(column);
        status = new TextView(this);
        notice = new TextView(this);
        notice.setPadding(0, pad, 0, pad / 2);
        openSettings = new Button(this);
        openSettings.setText(R.string.open_app_settings);
        openSettings.setOnClickListener(v -> startActivity(
                new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", getPackageName(), null))));
        LinearLayout.LayoutParams wrap = new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.WRAP_CONTENT, LinearLayout.LayoutParams.WRAP_CONTENT);
        wrap.gravity = Gravity.END;
        column.addView(status);
        column.addView(notice);
        column.addView(openSettings, wrap);
        setContentView(frame);

        String version = Core.version();
        Log.i(TAG, "core " + version);
        String started;
        try {
            started = Core.start(getFilesDir().getPath(), DataKey.load(this), deviceName(), Build.VERSION.RELEASE,
                    DevHarness.options(this));
        } catch (Exception e) {
            // The Keystore did not give the data key back: nothing sealed can be read. Said, never worked around.
            started = "ERROR the data key: " + e;
        }
        Log.i(TAG, "CORE " + started);
        status.setText("Swapdesk " + version + "\n" + started);
        Networks.start(this);
        DevHarness.run(this, status);
        View screen = DevHarness.screen(this);
        if (screen != null) {
            FrameLayout holder = new FrameLayout(this);
            holder.setFitsSystemWindows(true);
            holder.addView(screen);
            setContentView(holder);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        // Also on the way back from the settings of the system, where the person may have changed the answer.
        showLocalNetwork();
    }

    /**
     * Asks for the local network once, where it is a permission, and says what its refusal stops. Once means once:
     * the answer is remembered across the restarts of this screen (a rotation, a change of language) and of the
     * app, and after a refusal the way back is the settings of the system, never the question again.
     */
    private void showLocalNetwork() {
        boolean needed = Build.VERSION.SDK_INT >= LOCAL_NETWORK_FROM;
        boolean granted = !needed || checkSelfPermission(LOCAL_NETWORK) == PackageManager.PERMISSION_GRANTED;
        boolean asked = getPreferences(MODE_PRIVATE).getBoolean(ASKED_LOCAL_NETWORK, false);
        if (needed && !granted && !asked) {
            getPreferences(MODE_PRIVATE).edit().putBoolean(ASKED_LOCAL_NETWORK, true).apply();
            Log.i(TAG, "LOCAL_NETWORK asking");
            notice.setText(R.string.local_network_why);
            openSettings.setVisibility(View.GONE);
            requestPermissions(new String[] {LOCAL_NETWORK}, ASK_LOCAL_NETWORK);
            return;
        }
        Log.i(TAG, "LOCAL_NETWORK " + (!needed ? "not_needed" : granted ? "granted" : "refused"));
        notice.setText(granted ? "" : getString(R.string.local_network_refused));
        notice.setVisibility(granted ? View.GONE : View.VISIBLE);
        openSettings.setVisibility(granted ? View.GONE : View.VISIBLE);
    }

    @Override
    public void onRequestPermissionsResult(int request, String[] permissions, int[] results) {
        super.onRequestPermissionsResult(request, permissions, results);
        if (request == ASK_LOCAL_NETWORK) {
            showLocalNetwork();
        }
    }

    /** The name the person gave the phone, or its model. */
    private String deviceName() {
        String name = Settings.Global.getString(getContentResolver(), Settings.Global.DEVICE_NAME);
        return name == null || name.trim().isEmpty() ? Build.MODEL : name.trim();
    }
}
