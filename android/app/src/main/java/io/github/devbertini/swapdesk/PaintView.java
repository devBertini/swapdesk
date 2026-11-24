package io.github.devbertini.swapdesk;

import android.content.Context;
import android.graphics.BlendMode;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.DashPathEffect;
import android.graphics.Paint;
import android.graphics.Path;
import android.text.TextPaint;
import android.util.Log;
import android.view.View;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;

/**
 * The view of the shared screens. It decides nothing about what a screen looks like: the core paints the screen
 * into a buffer of operations, in device pixels, and this view replays them on its canvas (crates/
 * swapdesk-shell-android, {@code canvas_ops}, whose {@code decode} this follows field for field; every number
 * little-endian, a colour {@code 0xAARRGGBB}, a rectangle x, y, width, height):
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
 */
final class PaintView extends View {
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

    private final Fonts fonts;
    private final TextMeasurer measurer;
    private final TextPaint text = Fonts.paint();
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Path path = new Path();
    private DashPathEffect dashes;
    private float dashesWidth;

    /** Development only: the size in DIPs of a desktop window to fit into the view, or 0 for the phone's own. */
    private float fitWidth;
    private float fitHeight;
    /** Development only: how many more frames are painted and logged with what they cost. */
    private int traced;

    PaintView(Context context) {
        super(context);
        fonts = new Fonts(Shell.font());
        measurer = new TextMeasurer(fonts);
        float[] mono = fonts.monoMetrics();
        Shell.monoMetrics(mono[0], mono[1]);
    }

    /** Development only: shows a window of this size in DIPs whole, as large as the view allows. */
    void fit(float width, float height) {
        fitWidth = width;
        fitHeight = height;
    }

    /** Development only: paints this many frames in a row and logs what each cost. */
    void trace(int frames) {
        traced = frames;
        invalidate();
    }

    @Override
    protected void onDraw(Canvas canvas) {
        long start = System.nanoTime();
        float density = getResources().getDisplayMetrics().density;
        int width = getWidth();
        int height = getHeight();
        if (fitWidth > 0f && fitHeight > 0f) {
            density = Math.min(width / fitWidth, height / fitHeight);
            width = Math.round(fitWidth * density);
            height = Math.round(fitHeight * density);
        }
        long measuring = 0;
        byte[] frame = Shell.paint(width, height, density);
        for (int list = 0; frame.length > 0 && frame[0] != FRAME && list < LISTS_MAX; list++) {
            long from = System.nanoTime();
            byte[] answer;
            try {
                answer = measurer.measure(frame);
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
            frame = Shell.paint(width, height, density);
        }
        long painted = System.nanoTime();
        int operations = frame.length > 0 && frame[0] == FRAME ? replay(canvas, frame) : -1;
        if (traced > 0) {
            traced--;
            long end = System.nanoTime();
            Log.i(TAG, "FRAME " + Shell.cost() + " paint_us=" + (painted - start - measuring) / 1000 + " measure_us="
                    + measuring / 1000 + " replay_us=" + (end - painted) / 1000 + " bytes=" + frame.length
                    + " operations=" + operations + " view=" + width + "x" + height + " density=" + density);
            if (traced > 0) {
                postInvalidateOnAnimation();
            }
        }
    }

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
                        int length = in.getShort() & 0xFFFF;
                        String run = new String(frame, in.position(), length, StandardCharsets.UTF_8);
                        in.position(in.position() + length);
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
