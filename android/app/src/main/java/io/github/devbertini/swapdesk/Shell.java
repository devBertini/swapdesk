package io.github.devbertini.swapdesk;

import android.os.Handler;
import android.os.Looper;

/**
 * The shared screens and the shell around them, in the Rust core (crates/swapdesk-app-android,
 * {@code phone_screen}; crates/swapdesk-shell-android, {@code PhoneShell}): the same screens Windows and the Mac
 * show. Every native method is called from the main thread, where the core keeps the screen. Points and lengths
 * are device pixels of the view.
 */
final class Shell {
    static {
        System.loadLibrary("swapdesk_android");
    }

    private static final Handler MAIN = new Handler(Looper.getMainLooper());
    private static Runnable news;

    private Shell() {}

    /** Lets the core call {@link #wake()}: it keeps this class. Once, before the first frame. */
    static native void attach();

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

    /**
     * Shows the phone's own screens over its service: the state starts in this language ("pt..." is Portuguese of
     * Brazil, anything else English) and appearance. After {@link Core#start}. A refusal starts with "ERROR ".
     */
    static native String showLive(String language, boolean dark);

    /**
     * What Android says about the local network now: 0 allowed (or a system that does not ask), 1 the system's
     * question is on the screen, 2 refused. The screens show their notice for 1 and 2.
     */
    static native void localNetwork(int state);

    /** Whether something the screens show is still on its way from the service. */
    static native boolean waitsForData();

    /** What still waits for the service shows as placeholders (100 ms after it was asked for). */
    static native void armPlaceholders();

    /** Whether the screen is behind the state: a frame is wanted. */
    static native boolean wantsFrame();

    /** A finger is down at this point and stays: the control under it shows as pressed. */
    static native void press(float x, float y);

    /** The finger came up where it went down: a tap on what is under it. */
    static native void release(float x, float y);

    /** The touch became a drag or was taken away: nothing stays pressed. */
    static native void cancel();

    /** Whether a drag that starts at this point moves the pane. */
    static native boolean scrollsAt(float x, float y);

    /** Moves the pane by {@code dy} (a finger moving up scrolls down). Returns whether it moved. */
    static native boolean scrollBy(float dy);

    /** Puts the pane at {@code y} from its top, inside its range. Returns whether it moved. */
    static native boolean scrollTo(float y);

    /** The pane's offset. */
    static native float scrollOffset();

    /** The pane's largest offset. */
    static native float scrollMax();

    /** Whether Android's back gesture is the app's to answer now (not on the first pane with nothing over it). */
    static native boolean takesBack();

    /** Android's back gesture. False when it is the system's. */
    static native boolean back();

    /** A second went by. */
    static native void tick();

    /** The core has news: applies them. Called by {@link #wake()}'s post. */
    static native void drain();

    /** The phone's time and its offset from UTC in minutes, for what the screens say about time. */
    static native void setClock(long nowMs, int utcOffsetMin);

    /** The system's light or dark appearance. */
    static native void setDark(boolean dark);

    /**
     * What only Android can do, queued since the last call (crates/swapdesk-shell-android,
     * {@code PhoneShell::take_asks}; {@link PaintView} runs it).
     */
    static native byte[] takeAsks();

    /** The native text fields of the last frame ({@code PhoneShell::edits}; {@link Fields} reads it). */
    static native byte[] edits();

    /** Scrolls the pane so that a text field is in sight (the keyboard took part of the screen). Whether it moved. */
    static native boolean reveal(int field);

    /** The text of a native field, when it acts. */
    static native void submitEdit(int field, String text);

    /** The answer to a list of choices; a negative index when it was closed without one. */
    static native void chose(int index);

    /**
     * Called by the core from any of its threads when it has news (its one call into Java, crates/
     * swapdesk-app-android, {@code ui_waker}): the main thread drains them and tells the screen.
     */
    @SuppressWarnings("unused") // called from Rust by name: keep the name and the signature
    static void wake() {
        MAIN.post(Shell::woken);
    }

    private static void woken() {
        drain();
        Runnable listener = news;
        if (listener != null) {
            listener.run();
        }
    }

    /** Who is told, on the main thread, after news were applied; null for nobody. */
    static void onNews(Runnable listener) {
        news = listener;
    }
}
