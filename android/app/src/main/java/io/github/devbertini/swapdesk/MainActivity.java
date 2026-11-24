package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.app.KeyguardManager;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.WindowInsets;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.TextView;

import java.util.Locale;

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

    /** The core runs. */
    private static final int STARTED = 0;
    /** What the app stored cannot be opened again ({@link DataKey.Unreadable}, or the core saying so). */
    private static final int UNREADABLE = 1;
    /** The core did not start this time. */
    private static final int NOT_STARTED = 2;

    /** Whether the core started, and if not, which card the screen shows ({@link Shell#showStopped}). */
    private int stopped = STARTED;
    /** This activity was in front once already: a later return tries a start that failed again. */
    private boolean resumedOnce;
    private TextView status;
    private TextView notice;
    private Button openSettings;
    /** The shared screens, once they are what the app shows. */
    private PaintView screen;

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
        } catch (DataKey.Unreadable e) {
            // What the app stored cannot be opened again. Said on the screen, never worked around: nothing here
            // erases or replaces a file.
            started = "ERROR unreadable: the data key: " + e.getMessage();
        } catch (Exception e) {
            // The key store did not hand the data key over this time.
            started = "ERROR the data key: " + e;
        }
        Log.i(TAG, "CORE " + started);
        status.setText("Swapdesk " + version + "\n" + started);
        boolean running = started.startsWith("started") || started.contains("already running");
        if (running) {
            // Before anything can ask for a pairing: without a secure screen lock the core enters none.
            tellScreenLock();
        } else {
            stopped = started.startsWith("ERROR unreadable") ? UNREADABLE : NOT_STARTED;
        }
        Networks.start(this);
        DevHarness.run(this, status);
        screen = DevHarness.screen(this);
        if (screen == null && !running && !DevHarness.scripted(this)) {
            // No core, so no service to show: the one card that says what happened, and what to do about it.
            int night = getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK;
            Shell.showStopped(Locale.getDefault().toLanguageTag(), night == Configuration.UI_MODE_NIGHT_YES,
                    stopped == UNREADABLE);
            Log.i(TAG, "SCREEN stopped " + (stopped == UNREADABLE ? "unreadable" : "not started"));
            screen = new PaintView(this);
            DevHarness.watch(this, screen);
        }
        if (screen == null && running && !DevHarness.scripted(this)) {
            // The app's own screens, over its own service.
            int night = getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK;
            String shown = Shell.showLive(Locale.getDefault().toLanguageTag(), night == Configuration.UI_MODE_NIGHT_YES);
            Log.i(TAG, "SCREEN " + shown);
            if (!shown.startsWith("ERROR")) {
                screen = new PaintView(this);
                DevHarness.watch(this, screen);
            }
        }
        if (screen != null) {
            // The screens keep clear of the system's bars, of a cutout and of the keyboard: the holder takes them
            // as its padding, and shows the screen's own ground behind them.
            FrameLayout holder = new FrameLayout(this);
            holder.setOnApplyWindowInsetsListener((view, insets) -> {
                Insets around = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout()
                        | WindowInsets.Type.ime());
                view.setPadding(around.left, around.top, around.right, around.bottom);
                return WindowInsets.CONSUMED;
            });
            holder.addView(screen);
            setContentView(holder);
        }
    }

    /**
     * Android 11 and 12 tell the back gesture this way; from Android 13 the screen registers a callback while the
     * gesture is the app's ({@link PaintView}).
     */
    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (screen == null || !screen.back()) {
            super.onBackPressed();
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (stopped == NOT_STARTED && resumedOnce) {
            // The person came back to an app that did not start: it tries again from the beginning.
            recreate();
            return;
        }
        resumedOnce = true;
        if (stopped != STARTED) {
            // No core: nothing asks Android for anything, or its dialog would open over the card.
            return;
        }
        // Also on the way back from the settings of the system, where the person may have changed the answers.
        tellScreenLock();
        showLocalNetwork();
    }

    /**
     * Tells the core whether the phone has a secure screen lock (a PIN, a pattern or a password): without one it
     * enters no pairing, and "Parear" says what to set ({@link Core#screenLock}).
     */
    private void tellScreenLock() {
        KeyguardManager keyguard = getSystemService(KeyguardManager.class);
        boolean secure = DevHarness.secure(this, keyguard != null && keyguard.isDeviceSecure());
        Log.i(TAG, "SCREEN_LOCK " + (secure ? "secure" : "none"));
        Core.screenLock(secure);
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
            tellScreens(1);
            requestPermissions(new String[] {LOCAL_NETWORK}, ASK_LOCAL_NETWORK);
            return;
        }
        Log.i(TAG, "LOCAL_NETWORK " + (!needed ? "not_needed" : granted ? "granted" : "refused"));
        tellScreens(granted ? 0 : 2);
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

    /** The shared screens show the notice of the local network themselves ({@link Shell#localNetwork}). */
    private void tellScreens(int localNetwork) {
        if (screen != null) {
            Shell.localNetwork(localNetwork);
            screen.requestFrame();
        }
    }

    /** The name the person gave the phone, or its model. */
    private String deviceName() {
        String name = Settings.Global.getString(getContentResolver(), Settings.Global.DEVICE_NAME);
        return name == null || name.trim().isEmpty() ? Build.MODEL : name.trim();
    }
}
