package io.github.devbertini.swapdesk;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.res.Configuration;
import android.graphics.BlendMode;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.DashPathEffect;
import android.graphics.Paint;
import android.graphics.Path;
import android.net.Uri;
import android.os.Build;
import android.text.TextPaint;
import android.util.Log;
import android.view.Choreographer;
import android.view.GestureDetector;
import android.view.MotionEvent;
import android.widget.FrameLayout;
import android.widget.OverScroller;
import android.window.OnBackInvokedCallback;
import android.window.OnBackInvokedDispatcher;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.TimeZone;

/**
 * The view of the shared screens. It decides nothing about what a screen looks like or does: the core paints the
 * screen into a buffer of operations, in device pixels, and this view replays them on its canvas; a finger's tap
 * and drag go to the core as what they are (Android recognises them, with its own slop and fling), and what only
 * Android can do comes back as asks (a list of choices, the keyboard in a field, a page in the browser).
 *
 * <p>A frame is built before the draw pass (a Choreographer callback), so the native text fields ({@link Fields})
 * move in the same frame as the boxes drawn under them.
 *
 * <p>The frame (crates/swapdesk-shell-android, {@code canvas_ops}, whose {@code decode} this follows field for
 * field; every number little-endian, a colour {@code 0xAARRGGBB}, a rectangle x, y, width, height):
 *
 * <pre>
 *  1 clear            colour
 *  2 fill rect        rect, colour
 *  3 fill rrect       rect, radius, colour
 *  4 stroke rrect     rect, radius, width, colour        square ends, mitred corners, centred on the rect
 *  5 the same, dashed                                    dashes and gaps two widths long
 *  6 line             from, to, width, colour, u8 round ends
 *  7 fill circle      centre, radius, colour
 *  8 stroke circle    centre, radius, width, colour
 *  9 stroke path      width, colour, u16 steps, each u8 kind (0 move x y, 1 line x y, 2 cubic x1 y1 x2 y2 x y,
 *                     3 close)                           round ends and joins
 * 10 triangle         three points, colour
 * 11 shadow layer     rect, radius, colour 0x00RRGGBB, f32 alpha
 * 12 push clip        rect
 * 13 pop clip
 * 14 text             baseline start x y, colour, f32 size, u16 weight, f32 tracking (ems), u8 flags,
 *                     u16 length, the text (UTF-8)
 * </pre>
 *
 * <p>The asks ({@code PhoneShell::take_asks}): u8 count, then per ask a u8 code and: 1 a list of choices (u16
 * selected or 0xFFFF, u16 count, the choices); 2 the keyboard in a field (u8 field); 3 a field's text to the core
 * (u8 field, u8 clear afterwards); 4 a page for the browser (its address); 5 a note for the log (its text); 6 the
 * app's page in the system's settings (nothing more); 7 the system's security settings (nothing more). A string
 * is its u16 length and its UTF-8 bytes.
 */
final class PaintView extends FrameLayout {
    private static final String TAG = "Swapdesk";

    private static final int FRAME = 0;
    /** The core gives a frame after six lists of text at most; this view never asks more often than that. */
    private static final int LISTS_MAX = 8;

    private static final int CLEAR = 1;
    private static final int FILL_RECT = 2;
    private static final int FILL_RRECT = 3;
    private static final int STROKE_RRECT = 4;
    private static final int STROKE_RRECT_DASHED = 5;
    private static final int LINE = 6;
    private static final int FILL_CIRCLE = 7;
    private static final int STROKE_CIRCLE = 8;
    private static final int STROKE_PATH = 9;
    private static final int TRIANGLE = 10;
    private static final int SHADOW_LAYER = 11;
    private static final int PUSH_CLIP = 12;
    private static final int POP_CLIP = 13;
    private static final int TEXT = 14;

    private static final int SEG_MOVE = 0;
    private static final int SEG_LINE = 1;
    private static final int SEG_CUBIC = 2;
    private static final int SEG_CLOSE = 3;

    private static final int ASK_MENU = 1;
    private static final int ASK_FOCUS_EDIT = 2;
    private static final int ASK_SUBMIT_EDIT = 3;
    private static final int ASK_OPEN_PAGE = 4;
    private static final int ASK_NOTE = 5;
    private static final int ASK_OPEN_APP_SETTINGS = 6;
    private static final int ASK_OPEN_SECURITY_SETTINGS = 7;

    private final Activity activity;
    private final Fonts fonts;
    private final TextMeasurer measurer;
    private final Fields fields;
    private final TextPaint text = Fonts.paint();
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Path path = new Path();
    private DashPathEffect dashes;
    private float dashesWidth;

    private final GestureDetector gestures;
    private final OverScroller fling;
    /** The control under the finger shows as pressed. */
    private boolean pressed;
    /** The touch that is down started where the pane scrolls. */
    private boolean scrolls;

    /** The last frame the core painted, replayed at every draw. */
    private byte[] frame;
    private boolean frameQueued;
    private final Choreographer.FrameCallback onFrame = nanos -> {
        frameQueued = false;
        buildFrame();
    };
    private final Runnable everySecond = this::second;
    private boolean ticking;
    private boolean placeholdersPosted;
    private final Runnable placeholders = () -> {
        placeholdersPosted = false;
        Shell.armPlaceholders();
        requestFrame();
    };
    /** Android 13 and later: registered while the back gesture is the app's to answer. */
    private Object backCallback;
    private boolean backRegistered;
    /** The canvas colour of the last frame: what shows behind the system's bars. */
    private int ground = Color.TRANSPARENT;

    /** The screens follow the phone's clock and its light or dark appearance. Off for a development mock. */
    private boolean followsSystem = true;
    /** Development only: the size in DIPs of a desktop window to fit into the view, or 0 for the phone's own. */
    private float fitWidth;
    private float fitHeight;
    /** Development only: how many more frames are painted and logged with what they cost. */
    private int traced;
    /** Development only: run after every frame. */
    private Runnable afterFrame;

    PaintView(Activity activity) {
        super(activity);
        this.activity = activity;
        setWillNotDraw(false);
        Shell.attach();
        fonts = new Fonts(Shell.font());
        measurer = new TextMeasurer(fonts);
        float[] mono = fonts.monoMetrics();
        Shell.monoMetrics(mono[0], mono[1]);
        fields = new Fields(this, fonts, this::acted);
        fling = new OverScroller(activity);
        gestures = new GestureDetector(activity, new Touch());
        // A hold is not a gesture of its own yet (context menus come with the files): a finger held and lifted
        // where it went down is a tap, as on any Android button.
        gestures.setIsLongpressEnabled(false);
    }

    /** Development only: shows a window of this size in DIPs whole, as large as the view allows. */
    void fit(float width, float height) {
        fitWidth = width;
        fitHeight = height;
    }

    /** Development only: paints this many frames in a row and logs what each cost. */
    void trace(int frames) {
        traced = frames;
        requestFrame();
    }

    /** Development only: runs this after every frame the core painted. */
    void afterFrame(Runnable hook) {
        afterFrame = hook;
    }

    /** Development only: a mock keeps its own clock and theme, so its pictures compare with the desktops'. */
    void keepScenario() {
        followsSystem = false;
    }

    // ----- frames -----

    /** Asks for a frame at the next vsync; many requests before it make one frame. */
    void requestFrame() {
        if (!frameQueued) {
            frameQueued = true;
            Choreographer.getInstance().postFrameCallback(onFrame);
        }
    }

    /** After something was done to the core (a tap, back, a choice, a field's text): its asks, then the screen. */
    private void acted() {
        runAsks();
        requestFrame();
        // What the screens asked the service for and did not get within 100 ms shows as placeholders (R-UI-1).
        if (!placeholdersPosted && Shell.waitsForData()) {
            placeholdersPosted = true;
            postDelayed(placeholders, 100);
        }
    }

    private float density() {
        if (fitWidth > 0f && fitHeight > 0f) {
            return Math.min(getWidth() / fitWidth, getHeight() / fitHeight);
        }
        return getResources().getDisplayMetrics().density;
    }

    /** Has the core paint a frame for the view as it is now, places the text fields over it and draws it. */
    private void buildFrame() {
        if (getWidth() <= 0 || getHeight() <= 0) {
            return;
        }
        long start = System.nanoTime();
        if (!fling.isFinished() && fling.computeScrollOffset()) {
            Shell.scrollTo(fling.getCurrY());
        }
        float density = density();
        int width = getWidth();
        int height = getHeight();
        if (fitWidth > 0f && fitHeight > 0f) {
            width = Math.round(fitWidth * density);
            height = Math.round(fitHeight * density);
        }
        long measuring = 0;
        byte[] painted = null;
        // A field under the keyboard is scrolled into sight: one more paint when that moved the pane.
        for (int attempt = 0; attempt < 2; attempt++) {
            painted = Shell.paint(width, height, density);
            for (int list = 0; painted.length > 0 && painted[0] != FRAME && list < LISTS_MAX; list++) {
                long from = System.nanoTime();
                byte[] answer;
                try {
                    answer = measurer.measure(painted);
                } catch (RuntimeException e) {
                    // A list that does not read: the core is told so (an empty answer) and gives its frame as it is.
                    Log.w(TAG, "MEASURE the list did not read: " + e);
                    answer = new byte[0];
                }
                measuring += System.nanoTime() - from;
                String said = Shell.measured(answer);
                if (!said.isEmpty()) {
                    Log.w(TAG, "MEASURE " + said);
                }
                painted = Shell.paint(width, height, density);
            }
            int typing = fields.focused();
            if (typing == 0 || !Shell.reveal(typing)) {
                break;
            }
        }
        if (painted.length > 0 && painted[0] == FRAME) {
            frame = painted;
            fields.place(Shell.edits());
            followGround();
            followBack();
            if (afterFrame != null) {
                afterFrame.run();
            }
        }
        invalidate();
        if (traced > 0) {
            traced--;
            Log.i(TAG, "FRAME " + Shell.cost() + " paint_us=" + (System.nanoTime() - start - measuring) / 1000
                    + " measure_us=" + measuring / 1000 + " bytes=" + painted.length + " view=" + width + "x" + height
                    + " density=" + density);
        }
        if (traced > 0 || !fling.isFinished() || Shell.wantsFrame()) {
            requestFrame();
        }
    }

    @Override
    protected void onSizeChanged(int width, int height, int oldWidth, int oldHeight) {
        super.onSizeChanged(width, height, oldWidth, oldHeight);
        requestFrame();
    }

    @Override
    protected void onDraw(Canvas canvas) {
        if (frame == null) {
            // The first draw came before the first frame callback.
            buildFrame();
        }
        if (frame != null) {
            long start = System.nanoTime();
            int operations = replay(canvas, frame);
            if (traced > 0) {
                Log.i(TAG, "REPLAY replay_us=" + (System.nanoTime() - start) / 1000 + " operations=" + operations);
            }
        }
    }

    // ----- the phone around the screens -----

    @Override
    protected void onAttachedToWindow() {
        super.onAttachedToWindow();
        Shell.onNews(this::acted);
        followSystem();
        // News that came before there was a view to tell (the answers to what the screens asked at their start).
        Shell.drain();
        acted();
        if (!ticking) {
            ticking = true;
            postDelayed(everySecond, 1000);
        }
        requestFrame();
    }

    @Override
    protected void onDetachedFromWindow() {
        Shell.onNews(null);
        removeCallbacks(everySecond);
        removeCallbacks(placeholders);
        placeholdersPosted = false;
        ticking = false;
        if (backRegistered && Build.VERSION.SDK_INT >= 33) {
            activity.getOnBackInvokedDispatcher().unregisterOnBackInvokedCallback((OnBackInvokedCallback) backCallback);
            backRegistered = false;
        }
        super.onDetachedFromWindow();
    }

    @Override
    protected void onConfigurationChanged(Configuration config) {
        super.onConfigurationChanged(config);
        followSystem();
        requestFrame();
    }

    /** Opens a page of the system's settings; a phone without that page gets the first page of its settings. */
    private void settings(Intent page) {
        try {
            activity.startActivity(page);
        } catch (android.content.ActivityNotFoundException missing) {
            Log.i(TAG, "no settings page for " + page.getAction());
            try {
                activity.startActivity(new Intent(android.provider.Settings.ACTION_SETTINGS));
            } catch (android.content.ActivityNotFoundException none) {
                Log.i(TAG, "no settings to open");
            }
        }
    }

    private void second() {
        followSystem();
        Shell.tick();
        runAsks();
        // A second that changed nothing on the screen paints nothing.
        if (Shell.wantsFrame()) {
            requestFrame();
        }
        postDelayed(everySecond, 1000);
    }

    /** The phone's clock and its light or dark appearance, for the screens. */
    private void followSystem() {
        if (!followsSystem) {
            return;
        }
        long now = System.currentTimeMillis();
        Shell.setClock(now, TimeZone.getDefault().getOffset(now) / 60_000);
        int night = getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK;
        Shell.setDark(night == Configuration.UI_MODE_NIGHT_YES);
    }

    /** What shows behind the system's bars is the screen's own ground, and the bars' icons are read on it. */
    private void followGround() {
        if (frame.length < 6 || frame[1] != CLEAR) {
            return;
        }
        int color = ByteBuffer.wrap(frame, 2, 4).order(ByteOrder.LITTLE_ENDIAN).getInt();
        if (color == ground) {
            return;
        }
        ground = color;
        ((android.view.View) getParent()).setBackgroundColor(color);
        boolean light = Color.luminance(color) > 0.5f;
        int bars = android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                | android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
        android.view.WindowInsetsController controller = activity.getWindow().getInsetsController();
        if (controller != null) {
            controller.setSystemBarsAppearance(light ? bars : 0, bars);
        }
    }

    /** Android's back gesture: the app's while the core takes it, the system's on the first pane. */
    boolean back() {
        if (!Shell.back()) {
            return false;
        }
        acted();
        return true;
    }

    /**
     * From Android 13 the system asks before the gesture ends whether the app answers it: the callback is
     * registered only while the core takes the gesture, so on the first pane the system shows its own way home.
     */
    private void followBack() {
        if (Build.VERSION.SDK_INT < 33) {
            return;
        }
        boolean takes = Shell.takesBack();
        if (takes == backRegistered) {
            return;
        }
        OnBackInvokedDispatcher dispatcher = activity.getOnBackInvokedDispatcher();
        if (backCallback == null) {
            backCallback = (OnBackInvokedCallback) this::back;
        }
        if (takes) {
            dispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT,
                    (OnBackInvokedCallback) backCallback);
        } else {
            dispatcher.unregisterOnBackInvokedCallback((OnBackInvokedCallback) backCallback);
        }
        backRegistered = takes;
    }

    // ----- a finger -----

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        gestures.onTouchEvent(event);
        int action = event.getActionMasked();
        if ((action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) && pressed) {
            // The finger came up after it left the place of a tap.
            Shell.cancel();
            pressed = false;
            requestFrame();
        }
        return true;
    }

    /**
     * The plain listener, not {@code SimpleOnGestureListener}: that one also listens for double taps, and the
     * detector then takes a second tap within 300 ms and 100 dp of the first as half of a double tap and never
     * reports it as a tap. Two quick taps here are two taps, on two controls or on one.
     */
    private final class Touch implements GestureDetector.OnGestureListener {
        @Override
        public void onLongPress(MotionEvent event) {}

        @Override
        public boolean onDown(MotionEvent event) {
            fling.forceFinished(true);
            pressed = false;
            scrolls = Shell.scrollsAt(event.getX(), event.getY());
            return true;
        }

        @Override
        public void onShowPress(MotionEvent event) {
            // Only once the finger has stayed: a drag that scrolls does not flash what it started on.
            press(event);
        }

        @Override
        public boolean onSingleTapUp(MotionEvent event) {
            press(event);
            Shell.release(event.getX(), event.getY());
            pressed = false;
            acted();
            return true;
        }

        @Override
        public boolean onScroll(MotionEvent down, MotionEvent event, float dx, float dy) {
            if (pressed) {
                Shell.cancel();
                pressed = false;
                requestFrame();
            }
            if (scrolls && Shell.scrollBy(dy)) {
                requestFrame();
            }
            return true;
        }

        @Override
        public boolean onFling(MotionEvent down, MotionEvent event, float vx, float vy) {
            if (scrolls) {
                fling.fling(0, Math.round(Shell.scrollOffset()), 0, Math.round(-vy), 0, 0, 0,
                        Math.round(Shell.scrollMax()));
                requestFrame();
            }
            return true;
        }

        private void press(MotionEvent event) {
            if (!pressed) {
                Shell.press(event.getX(), event.getY());
                pressed = true;
                requestFrame();
            }
        }
    }

    // ----- what only Android can do -----

    private void runAsks() {
        byte[] asks = Shell.takeAsks();
        if (asks.length < 2) {
            return;
        }
        try {
            ByteBuffer in = ByteBuffer.wrap(asks).order(ByteOrder.LITTLE_ENDIAN);
            for (int count = in.get() & 0xFF; count > 0; count--) {
                int code = in.get() & 0xFF;
                switch (code) {
                    case ASK_MENU: {
                        int selected = in.getShort() & 0xFFFF;
                        String[] choices = new String[in.getShort() & 0xFFFF];
                        for (int i = 0; i < choices.length; i++) {
                            choices[i] = string(in, asks);
                        }
                        choose(choices, selected < choices.length ? selected : -1);
                        break;
                    }
                    case ASK_FOCUS_EDIT:
                        fields.focus(in.get() & 0xFF);
                        break;
                    case ASK_SUBMIT_EDIT: {
                        int field = in.get() & 0xFF;
                        fields.submit(field, in.get() != 0);
                        break;
                    }
                    case ASK_OPEN_PAGE:
                        open(string(in, asks));
                        break;
                    case ASK_NOTE:
                        Log.i(TAG, "ASK " + string(in, asks));
                        break;
                    case ASK_OPEN_APP_SETTINGS:
                        Log.i(TAG, "ASK the app's settings");
                        settings(new Intent(android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                                Uri.fromParts("package", activity.getPackageName(), null)));
                        break;
                    case ASK_OPEN_SECURITY_SETTINGS:
                        Log.i(TAG, "ASK the security settings");
                        settings(new Intent(android.provider.Settings.ACTION_SECURITY_SETTINGS));
                        break;
                    default:
                        throw new IllegalStateException("ask " + code + " is not one this build knows");
                }
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "ASKS did not read: " + e);
        }
    }

    /** A select's choices, as the system's list over the screen; closing it without a choice is an answer too. */
    private void choose(String[] choices, int selected) {
        boolean[] answered = {false};
        new AlertDialog.Builder(activity)
                .setSingleChoiceItems(choices, selected, (dialog, which) -> {
                    answered[0] = true;
                    dialog.dismiss();
                    Shell.chose(which);
                    acted();
                })
                .setOnDismissListener(dialog -> {
                    if (!answered[0]) {
                        Shell.chose(-1);
                    }
                })
                .show();
    }

    private void open(String page) {
        try {
            activity.startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(page)));
        } catch (ActivityNotFoundException e) {
            Log.w(TAG, "ASK no app opens the page");
        }
    }

    private static String string(ByteBuffer in, byte[] from) {
        int length = in.getShort() & 0xFFFF;
        String text = new String(from, in.position(), length, StandardCharsets.UTF_8);
        in.position(in.position() + length);
        return text;
    }

    // ----- the replay -----

    /** Replays a frame. Returns how many operations it held, or -1 when it did not read to its end. */
    private int replay(Canvas canvas, byte[] frame) {
        ByteBuffer in = ByteBuffer.wrap(frame).order(ByteOrder.LITTLE_ENDIAN);
        in.get();
        int operations = 0;
        int saved = canvas.save();
        try {
            while (in.hasRemaining()) {
                int operation = in.get();
                operations++;
                switch (operation) {
                    case CLEAR:
                        canvas.drawColor(in.getInt(), BlendMode.SRC);
                        break;
                    case FILL_RECT: {
                        float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                        fill(in.getInt());
                        canvas.drawRect(x, y, x + w, y + h, paint);
                        break;
                    }
                    case FILL_RRECT: {
                        float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                        float radius = in.getFloat();
                        fill(in.getInt());
                        roundRect(canvas, x, y, w, h, radius);
                        break;
                    }
                    case STROKE_RRECT:
                    case STROKE_RRECT_DASHED: {
                        float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                        float radius = in.getFloat(), stroke = in.getFloat();
                        stroke(in.getInt(), stroke, false);
                        if (operation == STROKE_RRECT_DASHED && stroke > 0f) {
                            if (dashes == null || dashesWidth != stroke) {
                                dashes = new DashPathEffect(new float[] {2f * stroke, 2f * stroke}, 0f);
                                dashesWidth = stroke;
                            }
                            paint.setPathEffect(dashes);
                        }
                        roundRect(canvas, x, y, w, h, radius);
                        paint.setPathEffect(null);
                        break;
                    }
                    case LINE: {
                        float x1 = in.getFloat(), y1 = in.getFloat(), x2 = in.getFloat(), y2 = in.getFloat();
                        float stroke = in.getFloat();
                        int color = in.getInt();
                        stroke(color, stroke, in.get() != 0);
                        canvas.drawLine(x1, y1, x2, y2, paint);
                        break;
                    }
                    case FILL_CIRCLE: {
                        float x = in.getFloat(), y = in.getFloat(), radius = in.getFloat();
                        fill(in.getInt());
                        canvas.drawCircle(x, y, radius, paint);
                        break;
                    }
                    case STROKE_CIRCLE: {
                        float x = in.getFloat(), y = in.getFloat(), radius = in.getFloat(), stroke = in.getFloat();
                        stroke(in.getInt(), stroke, false);
                        canvas.drawCircle(x, y, radius, paint);
                        break;
                    }
                    case STROKE_PATH: {
                        float stroke = in.getFloat();
                        stroke(in.getInt(), stroke, true);
                        path.rewind();
                        for (int steps = in.getShort() & 0xFFFF; steps > 0; steps--) {
                            switch (in.get()) {
                                case SEG_MOVE:
                                    path.moveTo(in.getFloat(), in.getFloat());
                                    break;
                                case SEG_LINE:
                                    path.lineTo(in.getFloat(), in.getFloat());
                                    break;
                                case SEG_CUBIC:
                                    path.cubicTo(in.getFloat(), in.getFloat(), in.getFloat(), in.getFloat(),
                                            in.getFloat(), in.getFloat());
                                    break;
                                case SEG_CLOSE:
                                    path.close();
                                    break;
                                default:
                                    throw new IllegalStateException("a path step this build does not know");
                            }
                        }
                        canvas.drawPath(path, paint);
                        break;
                    }
                    case TRIANGLE: {
                        path.rewind();
                        path.moveTo(in.getFloat(), in.getFloat());
                        path.lineTo(in.getFloat(), in.getFloat());
                        path.lineTo(in.getFloat(), in.getFloat());
                        path.close();
                        fill(in.getInt());
                        canvas.drawPath(path, paint);
                        break;
                    }
                    case SHADOW_LAYER: {
                        float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                        float radius = in.getFloat();
                        int rgb = in.getInt();
                        float alpha = Math.max(0f, Math.min(in.getFloat(), 1f));
                        paint.setStyle(Paint.Style.FILL);
                        // An alpha finer than 1/255: twelve of these make one shadow.
                        paint.setColor(Color.pack(Color.red(rgb) / 255f, Color.green(rgb) / 255f, Color.blue(rgb) / 255f,
                                alpha));
                        roundRect(canvas, x, y, w, h, radius);
                        break;
                    }
                    case PUSH_CLIP: {
                        float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                        canvas.save();
                        canvas.clipRect(x, y, x + w, y + h);
                        break;
                    }
                    case POP_CLIP:
                        if (canvas.getSaveCount() > saved + 1) {
                            canvas.restore();
                        }
                        break;
                    case TEXT: {
                        float x = in.getFloat(), y = in.getFloat();
                        int color = in.getInt();
                        float size = in.getFloat();
                        int weight = in.getShort() & 0xFFFF;
                        float tracking = in.getFloat();
                        int flags = in.get() & 0xFF;
                        String run = string(in, frame);
                        fonts.apply(text, size, weight, tracking, flags);
                        text.setColor(color);
                        canvas.drawText(run, x, y, text);
                        break;
                    }
                    default:
                        throw new IllegalStateException("operation " + operation + " is not one this build knows");
                }
            }
            return operations;
        } catch (RuntimeException e) {
            // A frame that does not read to its end is a defect of the core: what read is shown, and it is said.
            Log.w(TAG, "REPLAY stopped at operation " + operations + ": " + e);
            return -1;
        } finally {
            canvas.restoreToCount(saved);
        }
    }

    private void fill(int color) {
        paint.setStyle(Paint.Style.FILL);
        paint.setColor(color);
    }

    private void stroke(int color, float width, boolean round) {
        paint.setStyle(Paint.Style.STROKE);
        paint.setColor(color);
        paint.setStrokeWidth(width);
        paint.setStrokeCap(round ? Paint.Cap.ROUND : Paint.Cap.BUTT);
        paint.setStrokeJoin(round ? Paint.Join.ROUND : Paint.Join.MITER);
    }

    /** A rounded rectangle with the current paint; nothing for an empty one, the radius at most half a side. */
    private void roundRect(Canvas canvas, float x, float y, float w, float h, float radius) {
        if (w <= 0f || h <= 0f) {
            return;
        }
        float r = Math.max(0f, Math.min(radius, Math.min(w, h) / 2f));
        canvas.drawRoundRect(x, y, x + w, y + h, r, r, paint);
    }
}
