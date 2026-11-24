package io.github.devbertini.swapdesk;

import android.graphics.Paint;
import android.graphics.Typeface;
import android.graphics.fonts.Font;
import android.graphics.fonts.FontFamily;
import android.graphics.fonts.FontStyle;
import android.text.TextPaint;
import android.util.Log;
import android.util.SparseArray;

import java.io.IOException;
import java.nio.ByteBuffer;

/**
 * The two faces of the interface: Instrument Sans at any weight (one variable font, read from the core, with the
 * system's faces behind it for the characters it lacks) and the phone's monospaced face. Text is measured and
 * drawn with paints made and set here, so both always agree.
 */
final class Fonts {
    /** The text flags of the core (crates/swapdesk-shell-android, {@code canvas_ops::flag}). */
    static final int MONO = 1;
    static final int ITALIC = 2;
    static final int ELLIPSIS = 4;
    /** The weight of the semi-bold start of a label, and the one the monospaced face turns bold at. */
    static final int SEMI_BOLD = 600;

    private static final String TAG = "Swapdesk";

    private final ByteBuffer sans;
    private final SparseArray<Typeface> sansByWeight = new SparseArray<>();
    private final Typeface[] mono = new Typeface[4];

    Fonts(byte[] instrumentSans) {
        // A font is read from a direct buffer only.
        sans = ByteBuffer.allocateDirect(instrumentSans.length);
        sans.put(instrumentSans);
        sans.rewind();
    }

    /** A paint for text: glyphs keep their exact places along the line and are not fitted to the pixel grid. */
    static TextPaint paint() {
        TextPaint paint = new TextPaint(Paint.ANTI_ALIAS_FLAG | Paint.SUBPIXEL_TEXT_FLAG);
        paint.setHinting(Paint.HINTING_OFF);
        return paint;
    }

    /** Sets a paint to a style: its size in device pixels, weight, tracking in ems and {@link #MONO}, {@link #ITALIC}. */
    void apply(Paint paint, float size, int weight, float tracking, int flags) {
        paint.setTypeface(face(weight, flags));
        paint.setTextSize(size);
        paint.setLetterSpacing(tracking);
    }

    /** The face of a style. The interface's face has no italic; the monospaced one is bold from 600. */
    Typeface face(int weight, int flags) {
        if ((flags & MONO) != 0) {
            boolean bold = weight >= SEMI_BOLD;
            boolean italic = (flags & ITALIC) != 0;
            int which = (bold ? 1 : 0) | (italic ? 2 : 0);
            if (mono[which] == null) {
                mono[which] = Typeface.create(Typeface.MONOSPACE, bold ? 700 : 400, italic);
            }
            return mono[which];
        }
        weight = Math.max(1, Math.min(weight, 1000));
        Typeface face = sansByWeight.get(weight);
        if (face == null) {
            face = sans(weight);
            sansByWeight.put(weight, face);
        }
        return face;
    }

    private Typeface sans(int weight) {
        try {
            Font font = new Font.Builder(sans.duplicate())
                    .setFontVariationSettings("'wght' " + weight)
                    .setWeight(weight)
                    .setSlant(FontStyle.FONT_SLANT_UPRIGHT)
                    .build();
            return new Typeface.CustomFallbackBuilder(new FontFamily.Builder(font).build())
                    .setStyle(new FontStyle(weight, FontStyle.FONT_SLANT_UPRIGHT))
                    .setSystemFallback("sans-serif")
                    .build();
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "FONT the interface's face did not load at " + weight + ": " + e);
            return Typeface.create(Typeface.SANS_SERIF, weight, false);
        }
    }

    /** The ascent and descent of the monospaced face, as fractions of its size. */
    float[] monoMetrics() {
        Paint paint = paint();
        paint.setTypeface(face(400, MONO));
        paint.setTextSize(1000);
        Paint.FontMetrics metrics = paint.getFontMetrics();
        return new float[] {-metrics.ascent / 1000f, metrics.descent / 1000f};
    }
}
