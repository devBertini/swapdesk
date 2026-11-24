package io.github.devbertini.swapdesk;

import android.graphics.Typeface;
import android.graphics.text.LineBreaker;
import android.text.Layout;
import android.text.SpannableString;
import android.text.Spanned;
import android.text.StaticLayout;
import android.text.TextPaint;
import android.text.TextUtils;
import android.text.style.MetricAffectingSpan;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Measures the text the core asks about. The core lays its screens out itself and only Android knows how wide
 * text is in the phone's fonts, so a frame that meets text it has no measure of comes back as a list of needs,
 * answered here in one go (crates/swapdesk-shell-android, {@code text_measure}; every number little-endian):
 *
 * <pre>
 * needs:  u8 1, u16 count, then per label:
 *           f32 size (device pixels), u16 weight, f32 tracking (ems), u8 flags (1 mono, 2 italic, 4 ellipsis),
 *           f32 width to lay out in (device pixels), u16 semi-bold start (an index into the string),
 *           u16 length, the text (UTF-8)
 * answer: u16 count, then per label, in the same order:
 *           u16 lines, then per line: u8 runs, then per run:
 *             u8 bold (0 or 1), f32 width (device pixels), u16 length, text (UTF-8)
 * </pre>
 *
 * A label is wrapped at its width, or with the ellipsis flag kept on one line and trimmed with "…". Each line comes
 * back as what is drawn on it, without the spaces it broke at, split where the semi-bold start ends.
 */
final class TextMeasurer {
    /** A label laid out in a width of nothing breaks at every character: no more lines than this are told. */
    private static final int LINES_MAX = 1000;

    private final Fonts fonts;
    private final TextPaint paint = Fonts.paint();

    TextMeasurer(Fonts fonts) {
        this.fonts = fonts;
    }

    /** The answer to a list of needs. Throws on a list that does not read (a defect of the core, never of a person). */
    byte[] measure(byte[] needs) {
        ByteBuffer in = ByteBuffer.wrap(needs).order(ByteOrder.LITTLE_ENDIAN);
        in.get();
        int count = in.getShort() & 0xFFFF;
        Out out = new Out(needs.length * 2 + 64);
        out.u16(count);
        for (int i = 0; i < count; i++) {
            float size = in.getFloat();
            int weight = in.getShort() & 0xFFFF;
            float tracking = in.getFloat();
            int flags = in.get() & 0xFF;
            float width = in.getFloat();
            int bold = in.getShort() & 0xFFFF;
            int length = in.getShort() & 0xFFFF;
            String text = new String(needs, in.position(), length, StandardCharsets.UTF_8);
            in.position(in.position() + length);

            fonts.apply(paint, size, weight, tracking, flags);
            bold = Math.min(bold, text.length());
            CharSequence styled = text;
            if (bold > 0) {
                SpannableString spanned = new SpannableString(text);
                Typeface face = fonts.face(Math.max(weight, Fonts.SEMI_BOLD), flags);
                spanned.setSpan(new FaceSpan(face), 0, bold, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                styled = spanned;
            }
            if ((flags & Fonts.ELLIPSIS) != 0) {
                oneLine(out, styled, bold, width);
            } else {
                wrapped(out, styled, bold, width);
            }
        }
        return out.done();
    }

    private void oneLine(Out out, CharSequence text, int bold, float width) {
        int end = TextUtils.indexOf(text, '\n');
        CharSequence first = end < 0 ? text : text.subSequence(0, end);
        CharSequence shown = TextUtils.ellipsize(first, paint, Math.max(0f, width), TextUtils.TruncateAt.END);
        out.u16(1);
        line(out, shown, 0, shown.length(), bold, Layout.getDesiredWidth(shown, paint));
    }

    private void wrapped(Out out, CharSequence text, int bold, float width) {
        StaticLayout layout = StaticLayout.Builder.obtain(text, 0, text.length(), paint, Math.max(1, Math.round(width)))
                .setIncludePad(false)
                .setBreakStrategy(LineBreaker.BREAK_STRATEGY_SIMPLE)
                .setHyphenationFrequency(Layout.HYPHENATION_FREQUENCY_NONE)
                .build();
        int lines = Math.max(1, Math.min(layout.getLineCount(), LINES_MAX));
        out.u16(lines);
        for (int i = 0; i < lines; i++) {
            if (i >= layout.getLineCount()) {
                line(out, text, 0, 0, bold, 0f);
            } else {
                line(out, text, layout.getLineStart(i), layout.getLineVisibleEnd(i), bold, layout.getLineMax(i));
            }
        }
    }

    /** One line of a label: its semi-bold part, when it has one, then the rest; their widths add up to the line's. */
    private void line(Out out, CharSequence text, int start, int end, int bold, float width) {
        int split = Math.max(start, Math.min(bold, end));
        boolean hasBold = split > start;
        boolean hasPlain = split < end || !hasBold;
        out.u8((hasBold ? 1 : 0) + (hasPlain ? 1 : 0));
        float boldWidth = !hasBold ? 0f : split < end ? Layout.getDesiredWidth(text, start, split, paint) : width;
        if (hasBold) {
            out.u8(1);
            out.f32(Math.max(0f, boldWidth));
            out.str(text.subSequence(start, split).toString());
        }
        if (hasPlain) {
            out.u8(0);
            out.f32(Math.max(0f, width - boldWidth));
            out.str(text.subSequence(split, end).toString());
        }
    }

    /** The semi-bold start of a label: another weight of the same face. */
    private static final class FaceSpan extends MetricAffectingSpan {
        private final Typeface face;

        FaceSpan(Typeface face) {
            this.face = face;
        }

        @Override
        public void updateMeasureState(TextPaint paint) {
            paint.setTypeface(face);
        }

        @Override
        public void updateDrawState(TextPaint paint) {
            paint.setTypeface(face);
        }
    }

    /** A buffer that grows, written little-endian. */
    private static final class Out {
        private byte[] bytes;
        private int length;

        Out(int capacity) {
            bytes = new byte[capacity];
        }

        private void room(int more) {
            if (length + more > bytes.length) {
                bytes = Arrays.copyOf(bytes, Math.max(bytes.length * 2, length + more));
            }
        }

        void u8(int value) {
            room(1);
            bytes[length++] = (byte) value;
        }

        void u16(int value) {
            room(2);
            bytes[length++] = (byte) value;
            bytes[length++] = (byte) (value >>> 8);
        }

        void f32(float value) {
            int bits = Float.floatToRawIntBits(Float.isFinite(value) ? value : 0f);
            room(4);
            bytes[length++] = (byte) bits;
            bytes[length++] = (byte) (bits >>> 8);
            bytes[length++] = (byte) (bits >>> 16);
            bytes[length++] = (byte) (bits >>> 24);
        }

        /** A string of at most 65535 bytes of UTF-8: a longer one is cut. */
        void str(String text) {
            byte[] utf8 = text.getBytes(StandardCharsets.UTF_8);
            if (utf8.length > 0xFFFF) {
                // 16000 of Java's units are at most 48000 bytes.
                utf8 = text.substring(0, 16_000).getBytes(StandardCharsets.UTF_8);
            }
            u16(utf8.length);
            room(utf8.length);
            System.arraycopy(utf8, 0, bytes, length, utf8.length);
            length += utf8.length;
        }

        byte[] done() {
            return Arrays.copyOf(bytes, length);
        }
    }
}
