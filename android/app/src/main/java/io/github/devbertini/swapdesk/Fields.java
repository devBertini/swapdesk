package io.github.devbertini.swapdesk;

import android.content.Context;
import android.os.Handler;
import android.os.Looper;
import android.text.Editable;
import android.text.InputType;
import android.text.TextWatcher;
import android.text.method.DigitsKeyListener;
import android.util.SparseArray;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * The native text fields over the drawn ones. The core draws a field's box and says where its text goes (crates/
 * swapdesk-shell-android, {@code PhoneShell::edits}; every number little-endian, device pixels of the view):
 *
 * <pre>
 * f32 x, y, width, height   the part of the view the fields show in (they are cut at its edges)
 * f32 text size, u32 ink, u32 placeholder ink (0xAARRGGBB)
 * u8 count, then per field: u8 field, f32 x, y, width, height, its value and its placeholder
 *                           (each u16 length, UTF-8)
 * </pre>
 *
 * Fields: 1 this device's name, 2 the chat's message, 3 a paired computer's new name, 4 an address to add,
 * 5 a file's new name. A field acts through {@link Shell#submitEdit}: on its keyboard's action key, the address
 * also 150 ms after the last key, this device's name also when the keyboard leaves it.
 */
final class Fields {
    static final int NAME = 1;
    static final int CHAT = 2;
    static final int RENAME = 3;
    static final int ADDRESS = 4;
    static final int FILE_RENAME = 5;

    /** The address field acts this long after the last key (R-UI-4). */
    private static final long ADDRESS_PAUSE_MS = 150;

    private final Context context;
    private final Fonts fonts;
    /** What is done after a field acted: the screen follows the state. */
    private final Runnable acted;
    private final FrameLayout layer;
    private final SparseArray<EditText> fields = new SparseArray<>();
    private final Handler main = new Handler(Looper.getMainLooper());
    private final Runnable addressPause = () -> submit(ADDRESS, false);
    private byte[] placed = new byte[0];
    /** A field is being filled by this class, not typed in. */
    private boolean filling;

    Fields(FrameLayout screen, Fonts fonts, Runnable acted) {
        this.context = screen.getContext();
        this.fonts = fonts;
        this.acted = acted;
        layer = new FrameLayout(context);
        layer.setClipChildren(true);
        screen.addView(layer, new FrameLayout.LayoutParams(0, 0));
    }

    /** The native field of a code, or null when the last frame has none. */
    EditText field(int code) {
        return fields.get(code);
    }

    /** Puts the fields where the last frame drew their boxes. Nothing moves when nothing changed. */
    void place(byte[] slots) {
        if (slots.length == 0 || Arrays.equals(slots, placed)) {
            return;
        }
        placed = slots;
        ByteBuffer in = ByteBuffer.wrap(slots).order(ByteOrder.LITTLE_ENDIAN);
        float viewX = in.getFloat(), viewY = in.getFloat(), viewW = in.getFloat(), viewH = in.getFloat();
        View screen = (View) layer.getParent();
        viewW = Math.min(viewW, screen.getWidth() - viewX);
        viewH = Math.min(viewH, screen.getHeight() - viewY);
        size(layer, Math.round(viewW), Math.round(viewH));
        layer.setTranslationX(viewX);
        layer.setTranslationY(viewY);
        float textSize = in.getFloat();
        int ink = in.getInt();
        int hint = in.getInt();

        boolean[] shown = new boolean[8];
        for (int count = in.get() & 0xFF; count > 0; count--) {
            int code = in.get() & 0xFF;
            float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
            String value = string(in, slots);
            String placeholder = string(in, slots);
            EditText field = fields.get(code);
            boolean fresh = field == null;
            if (fresh) {
                field = make(code);
                fields.put(code, field);
                layer.addView(field, new FrameLayout.LayoutParams(1, 1));
            }
            boolean appeared = fresh || field.getVisibility() != View.VISIBLE;
            field.setTextSize(TypedValue.COMPLEX_UNIT_PX, textSize);
            field.setTextColor(ink);
            field.setHintTextColor(hint);
            if (!placeholder.contentEquals(field.getHint() == null ? "" : field.getHint())) {
                field.setHint(placeholder);
            }
            size(field, Math.round(w), Math.round(h));
            field.setTranslationX(x - viewX);
            field.setTranslationY(y - viewY);
            // The state's value fills the field when it appears, and afterwards only while nobody types in it.
            if (appeared || (!field.hasFocus() && !value.contentEquals(field.getText()))) {
                filling = true;
                field.setText(value);
                filling = false;
            }
            if (appeared) {
                field.setVisibility(View.VISIBLE);
                if (code == RENAME || code == FILE_RENAME) {
                    // A rename opens with the keyboard in it and its text selected.
                    focus(code);
                    field.selectAll();
                }
            }
            if (code < shown.length) {
                shown[code] = true;
            }
        }
        for (int i = 0; i < fields.size(); i++) {
            int code = fields.keyAt(i);
            EditText field = fields.valueAt(i);
            if ((code >= shown.length || !shown[code]) && field.getVisibility() == View.VISIBLE) {
                if (field.hasFocus()) {
                    hideKeyboard(field);
                    field.clearFocus();
                }
                field.setVisibility(View.GONE);
            }
        }
    }

    /** Puts the keyboard in a field. */
    void focus(int code) {
        EditText field = fields.get(code);
        if (field == null || field.getVisibility() != View.VISIBLE) {
            return;
        }
        field.requestFocus();
        InputMethodManager keyboard = context.getSystemService(InputMethodManager.class);
        if (keyboard != null) {
            keyboard.showSoftInput(field, InputMethodManager.SHOW_IMPLICIT);
        }
    }

    /** Hands a field's text to the core; {@code clear} empties it afterwards (a message sent). */
    void submit(int code, boolean clear) {
        EditText field = fields.get(code);
        if (field == null) {
            return;
        }
        main.removeCallbacks(addressPause);
        Shell.submitEdit(code, field.getText().toString());
        if (clear) {
            filling = true;
            field.setText("");
            filling = false;
        }
        acted.run();
    }

    /** The field the keyboard is in, or 0. */
    int focused() {
        for (int i = 0; i < fields.size(); i++) {
            if (fields.valueAt(i).hasFocus()) {
                return fields.keyAt(i);
            }
        }
        return 0;
    }

    private EditText make(int code) {
        EditText field = new EditText(context);
        field.setBackground(null);
        field.setPadding(0, 0, 0, 0);
        field.setIncludeFontPadding(false);
        field.setSingleLine(true);
        field.setGravity(Gravity.CENTER_VERTICAL | Gravity.START);
        field.setTypeface(fonts.face(400, 0));
        field.setVisibility(View.GONE);
        if (code == ADDRESS) {
            // An IPv4 address: digits and dots.
            field.setKeyListener(DigitsKeyListener.getInstance("0123456789."));
            field.setImeOptions(EditorInfo.IME_ACTION_DONE);
        } else if (code == CHAT) {
            field.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
            field.setImeOptions(EditorInfo.IME_ACTION_SEND);
        } else {
            field.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_WORDS);
            field.setImeOptions(EditorInfo.IME_ACTION_DONE);
        }
        field.setOnEditorActionListener((view, action, event) -> {
            boolean enter = event != null && event.getKeyCode() == KeyEvent.KEYCODE_ENTER
                    && event.getAction() == KeyEvent.ACTION_DOWN;
            if (action != EditorInfo.IME_ACTION_DONE && action != EditorInfo.IME_ACTION_SEND && !enter) {
                return false;
            }
            submit(code, code == CHAT);
            if (code != CHAT) {
                hideKeyboard(view);
                view.clearFocus();
            }
            return true;
        });
        if (code == ADDRESS) {
            field.addTextChangedListener(new TextWatcher() {
                @Override
                public void beforeTextChanged(CharSequence text, int start, int count, int after) {}

                @Override
                public void onTextChanged(CharSequence text, int start, int before, int count) {}

                @Override
                public void afterTextChanged(Editable text) {
                    if (!filling) {
                        main.removeCallbacks(addressPause);
                        main.postDelayed(addressPause, ADDRESS_PAUSE_MS);
                    }
                }
            });
        }
        if (code == NAME) {
            field.setOnFocusChangeListener((view, has) -> {
                if (!has && view.getVisibility() == View.VISIBLE) {
                    submit(NAME, false);
                }
            });
        }
        return field;
    }

    private void hideKeyboard(View from) {
        InputMethodManager keyboard = context.getSystemService(InputMethodManager.class);
        if (keyboard != null) {
            keyboard.hideSoftInputFromWindow(from.getWindowToken(), 0);
        }
    }

    private static void size(View view, int width, int height) {
        FrameLayout.LayoutParams params = (FrameLayout.LayoutParams) view.getLayoutParams();
        width = Math.max(0, width);
        height = Math.max(0, height);
        if (params.width != width || params.height != height) {
            params.width = width;
            params.height = height;
            view.setLayoutParams(params);
        }
    }

    private static String string(ByteBuffer in, byte[] from) {
        int length = in.getShort() & 0xFFFF;
        String text = new String(from, in.position(), length, StandardCharsets.UTF_8);
        in.position(in.position() + length);
        return text;
    }
}
