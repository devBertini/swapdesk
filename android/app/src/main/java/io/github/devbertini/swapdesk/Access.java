package io.github.devbertini.swapdesk;

import android.graphics.Rect;
import android.os.Bundle;
import android.util.Log;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewParent;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityManager;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityNodeInfo.AccessibilityAction;
import android.view.accessibility.AccessibilityNodeProvider;
import android.widget.EditText;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * What a screen reader finds on the shared screens: the core's list of nodes for the last frame (crates/
 * swapdesk-shell-android, {@code access_nodes}; every number little-endian, rectangles in device pixels of the
 * view), answered as Android's virtual views of {@link PaintView}:
 *
 * <pre>
 * u8 pane (1 the page can scroll back, 2 forward)
 * u16 count, then per node: u32 id, u8 kind, u8 field (a text field's code, else 0), u16 flags,
 *                           f32 x, y, width, height, its label, description and value (each u16 length, UTF-8)
 * </pre>
 *
 * Kinds: 1 button, 2 tab, 3 switch, 4 check box, 5 link, 6 list of choices, 7 the native text field of the
 * node's field, 8 heading, 9 text, 10 progress, 11 table, 12 row, 13 cell, 14 column header. Flags: 1 clickable,
 * 2 checkable, 4 checked, 8 selected, 16 disabled, 32 the shell's focus. A text field is the real {@link
 * EditText} at its place in the order, labelled from its node; it has no virtual twin. Nothing here decides what
 * a node is or does: a click goes back to the core ({@link Shell#accessClick}) as a tap would.
 */
final class Access extends AccessibilityNodeProvider {
    private static final String TAG = "Swapdesk";
    private static final int BUTTON = 1;
    private static final int TAB = 2;
    private static final int SWITCH = 3;
    private static final int CHECK_BOX = 4;
    private static final int LINK = 5;
    private static final int COMBO_BOX = 6;
    private static final int EDIT = 7;
    private static final int HEADING = 8;
    private static final int PROGRESS = 10;

    private static final int CLICKABLE = 1;
    private static final int CHECKABLE = 2;
    private static final int CHECKED = 4;
    private static final int SELECTED = 8;
    private static final int DISABLED = 16;

    private static final int SCROLLS_BACK = 1;
    private static final int SCROLLS_FORWARD = 2;

    /** One node of the list. */
    private static final class Node {
        int id;
        int kind;
        int field;
        int flags;
        final Rect rect = new Rect();
        String label;
        String description;
        String value;
    }

    /** Asks the view's text fields for the native field of a code. */
    interface FieldOf {
        EditText field(int code);
    }

    private final View host;
    private final FieldOf fields;
    private final AccessibilityManager manager;
    private byte[] list = new byte[0];
    private Node[] nodes = new Node[0];
    private int pane;
    /** The node that has the reader's focus, or none. */
    private int focused = View.NO_ID;
    /** The node under a finger that explores the screen, or none. */
    private int hovered = View.NO_ID;

    Access(View host, FieldOf fields) {
        this.host = host;
        this.fields = fields;
        manager = host.getContext().getSystemService(AccessibilityManager.class);
    }

    /** Whether a reader listens: only then is the list asked for after a frame. */
    boolean listening() {
        return manager != null && manager.isEnabled();
    }

    /** The list of the frame just painted. Readers are told when it changed. */
    void update(byte[] bytes) {
        if (read(bytes) && listening()) {
            AccessibilityEvent changed = new AccessibilityEvent(AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED);
            changed.setContentChangeTypes(AccessibilityEvent.CONTENT_CHANGE_TYPE_SUBTREE);
            send(host, changed);
        }
    }

    /** Takes a list; whether it differs from the one held. */
    private boolean read(byte[] bytes) {
        if (Arrays.equals(bytes, list)) {
            return false;
        }
        Node[] read;
        int readPane;
        try {
            ByteBuffer in = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
            readPane = in.get() & 0xFF;
            int count = in.getShort() & 0xFFFF;
            read = new Node[count];
            for (int i = 0; i < count; i++) {
                Node n = new Node();
                n.id = in.getInt();
                n.kind = in.get() & 0xFF;
                n.field = in.get() & 0xFF;
                n.flags = in.getShort() & 0xFFFF;
                float x = in.getFloat(), y = in.getFloat(), w = in.getFloat(), h = in.getFloat();
                n.rect.set(Math.round(x), Math.round(y), Math.round(x + w), Math.round(y + h));
                n.label = string(in);
                n.description = string(in);
                n.value = string(in);
                read[i] = n;
            }
        } catch (RuntimeException e) {
            // A list that does not read leaves the last one in place.
            return false;
        }
        list = bytes;
        nodes = read;
        pane = readPane;
        labelFields();
        if (focused != View.NO_ID && node(focused) == null) {
            focused = View.NO_ID;
        }
        return true;
    }

    private static String string(ByteBuffer in) {
        int length = in.getShort() & 0xFFFF;
        byte[] bytes = new byte[length];
        in.get(bytes);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    private Node node(int id) {
        for (Node n : nodes) {
            if (n.id == id) {
                return n;
            }
        }
        return null;
    }

    /** Each native field says what it is for: its node's label, read with the text typed in it. */
    private void labelFields() {
        for (Node n : nodes) {
            EditText field = n.kind == EDIT ? fields.field(n.field) : null;
            if (field == null) {
                continue;
            }
            String label = n.label;
            field.setAccessibilityDelegate(new View.AccessibilityDelegate() {
                @Override
                public void onInitializeAccessibilityNodeInfo(View view, AccessibilityNodeInfo info) {
                    super.onInitializeAccessibilityNodeInfo(view, info);
                    info.setHintText(label);
                }
            });
        }
    }

    @Override
    public AccessibilityNodeInfo createAccessibilityNodeInfo(int id) {
        if (id == HOST_VIEW_ID) {
            // A reader that just came finds the screen as it is now, not as the last frame it was told of.
            read(Shell.access());
            AccessibilityNodeInfo info = new AccessibilityNodeInfo(host);
            host.onInitializeAccessibilityNodeInfo(info);
            info.setClassName("android.widget.ScrollView");
            for (Node n : nodes) {
                EditText field = n.kind == EDIT ? fields.field(n.field) : null;
                if (field != null) {
                    info.addChild(field);
                } else if (n.kind != EDIT) {
                    info.addChild(host, n.id);
                }
            }
            if ((pane & (SCROLLS_BACK | SCROLLS_FORWARD)) != 0) {
                info.setScrollable(true);
            }
            if ((pane & SCROLLS_BACK) != 0) {
                info.addAction(AccessibilityAction.ACTION_SCROLL_BACKWARD);
            }
            if ((pane & SCROLLS_FORWARD) != 0) {
                info.addAction(AccessibilityAction.ACTION_SCROLL_FORWARD);
            }
            return info;
        }
        Node n = node(id);
        if (n == null) {
            return null;
        }
        AccessibilityNodeInfo info = new AccessibilityNodeInfo(host, id);
        info.setPackageName(host.getContext().getPackageName());
        info.setClassName(className(n.kind));
        info.setParent(host);
        int[] at = new int[2];
        host.getLocationOnScreen(at);
        Rect screen = new Rect(n.rect);
        screen.offset(at[0], at[1]);
        info.setBoundsInScreen(screen);
        info.setVisibleToUser(true);
        info.setText(n.label);
        if (!n.description.isEmpty()) {
            info.setHintText(n.description);
        }
        if (!n.value.isEmpty()) {
            info.setStateDescription(n.value);
        }
        boolean enabled = (n.flags & DISABLED) == 0;
        info.setEnabled(enabled);
        if ((n.flags & CLICKABLE) != 0) {
            info.setFocusable(true);
            if (enabled) {
                info.setClickable(true);
                info.addAction(AccessibilityAction.ACTION_CLICK);
            }
        }
        info.setCheckable((n.flags & CHECKABLE) != 0);
        info.setChecked((n.flags & CHECKED) != 0);
        info.setSelected((n.flags & SELECTED) != 0);
        info.setHeading(n.kind == HEADING);
        if (focused == id) {
            info.setAccessibilityFocused(true);
            info.addAction(AccessibilityAction.ACTION_CLEAR_ACCESSIBILITY_FOCUS);
        } else {
            info.addAction(AccessibilityAction.ACTION_ACCESSIBILITY_FOCUS);
        }
        return info;
    }

    private static String className(int kind) {
        switch (kind) {
            case BUTTON:
            case TAB:
                return "android.widget.Button";
            case SWITCH:
                return "android.widget.Switch";
            case CHECK_BOX:
                return "android.widget.CheckBox";
            case LINK:
                return "android.widget.Button";
            case COMBO_BOX:
                return "android.widget.Spinner";
            case PROGRESS:
                return "android.widget.ProgressBar";
            default:
                return "android.widget.TextView";
        }
    }

    @Override
    public boolean performAction(int id, int action, Bundle arguments) {
        if (id == HOST_VIEW_ID) {
            if (action == AccessibilityNodeInfo.ACTION_SCROLL_FORWARD
                    || action == AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD) {
                float page = host.getHeight() * 0.8f;
                boolean moved = Shell.scrollBy(action == AccessibilityNodeInfo.ACTION_SCROLL_FORWARD ? page : -page);
                if (moved) {
                    ((PaintView) host).requestFrame();
                    AccessibilityEvent scrolled = new AccessibilityEvent(AccessibilityEvent.TYPE_VIEW_SCROLLED);
                    send(host, scrolled);
                }
                return moved;
            }
            return host.performAccessibilityAction(action, arguments);
        }
        Node n = node(id);
        if (n == null) {
            return false;
        }
        switch (action) {
            case AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS:
                if (Log.isLoggable(TAG, Log.DEBUG)) {
                    Log.d(TAG, "A11Y focus label=" + n.label);
                }
                if (focused != id) {
                    focused = id;
                    host.invalidate();
                    sendFor(n, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUSED);
                }
                return true;
            case AccessibilityNodeInfo.ACTION_CLEAR_ACCESSIBILITY_FOCUS:
                if (focused == id) {
                    focused = View.NO_ID;
                    host.invalidate();
                    sendFor(n, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUS_CLEARED);
                }
                return true;
            case AccessibilityNodeInfo.ACTION_CLICK:
                if ((n.flags & CLICKABLE) == 0 || (n.flags & DISABLED) != 0 || !Shell.accessClick(id)) {
                    return false;
                }
                if (Log.isLoggable(TAG, Log.DEBUG)) {
                    Log.d(TAG, "A11Y click label=" + n.label);
                }
                sendFor(n, AccessibilityEvent.TYPE_VIEW_CLICKED);
                ((PaintView) host).accessActed();
                return true;
            default:
                return false;
        }
    }

    /** A finger exploring the screen: the node under it is announced as it enters and leaves. */
    boolean hover(MotionEvent event) {
        if (!listening()) {
            return false;
        }
        int under = View.NO_ID;
        for (int i = nodes.length - 1; i >= 0; i--) {
            Node n = nodes[i];
            if (n.kind != EDIT && n.rect.contains((int) event.getX(), (int) event.getY())) {
                under = n.id;
                break;
            }
        }
        int action = event.getActionMasked();
        if (action == MotionEvent.ACTION_HOVER_EXIT) {
            under = View.NO_ID;
        }
        if (under != hovered) {
            Node was = node(hovered);
            hovered = under;
            Node now = node(under);
            if (now != null) {
                sendFor(now, AccessibilityEvent.TYPE_VIEW_HOVER_ENTER);
            }
            if (was != null) {
                sendFor(was, AccessibilityEvent.TYPE_VIEW_HOVER_EXIT);
            }
        }
        return under != View.NO_ID;
    }

    /** Development only: the node with this label, as the reader would find it, or -1. */
    int find(String label) {
        for (Node n : nodes) {
            if (n.kind != EDIT && n.label.equals(label)) {
                return n.id;
            }
        }
        return -1;
    }

    private void sendFor(Node n, int type) {
        if (!listening()) {
            return;
        }
        AccessibilityEvent event = new AccessibilityEvent(type);
        event.setSource(host, n.id);
        event.setPackageName(host.getContext().getPackageName());
        event.setClassName(className(n.kind));
        event.getText().add(n.label);
        event.setEnabled((n.flags & DISABLED) == 0);
        send(host, event);
    }

    /** Every event goes out here: none while no reader listens (Android then refuses it with an exception). */
    private void send(View host, AccessibilityEvent event) {
        if (!listening()) {
            return;
        }
        ViewParent parent = host.getParent();
        if (parent != null) {
            if (event.getPackageName() == null) {
                event.setPackageName(host.getContext().getPackageName());
                event.setSource(host);
            }
            parent.requestSendAccessibilityEvent(host, event);
        }
    }
}
