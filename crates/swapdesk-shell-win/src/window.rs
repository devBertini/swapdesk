//! A Win32 window of the shell: messages, the composition surface, hover, focus and keyboard, the native text
//! inputs, the AccessKit adapter and the timers. Drawing happens only in `WM_PAINT`, and only an invalidation asks
//! for one: an idle window does no work.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use accesskit::{Action, ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use swapdesk_ui::a11y;
use swapdesk_ui::debounce::{DEBOUNCE_MS, Debouncer};
use swapdesk_ui::tokens::{layout, palette};
use swapdesk_ui::trace::SlowGuard;
use swapdesk_ui::{ControlId, EditId, Effect, WindowKind};
use windows::Win32::Foundation::{COLORREF, D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, GetMonitorInfoW, HBRUSH, HDC, InvalidateRect, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromWindow, SetBkColor, SetTextColor,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::app::Shared;
use crate::gfx::{D2dPainter, Rect};
use crate::surface::Surface;
use crate::system::{WM_ACCESS, WM_EDIT_EVENT, WM_WAKE, edit_font, get_x, get_y, hiword, loword};
use crate::viewer::{KeyMsg, MouseKind, MouseMsg, PointerButton, ViewerHost};
use swapdesk_paint::{EditSlot, Frame, Interaction, Widget};

const WM_MOUSELEAVE: u32 = 0x02A3;
const CLASS_NAME: PCWSTR = w!("SwapdeskWindow");
pub const TIMER_TICK: usize = 1;
pub const TIMER_PLACEHOLDER: usize = 2;
/// The host banner's minute boundary (re-armed after each firing, stopped when the banner closes).
pub const TIMER_BANNER: usize = 4;
/// One-shot: 150 ms after the last key typed in the address field (R-UI-4).
const TIMER_ADDRESS: usize = 3;
const EN_CHANGE: u16 = 0x0300;
const EM_SETCUEBANNER: u32 = 0x1501;
const EM_SETSEL: u32 = 0x00B1;
const ES_AUTOHSCROLL_STYLE: u32 = 0x0080;

/// The EDIT window procedure the inputs had before they were subclassed.
pub(crate) static EDIT_ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);

struct EditHandle {
    id: EditId,
    hwnd: HWND,
    /// What was last applied, so a repaint that changes nothing sends nothing.
    rect: (i32, i32, i32, i32),
    font: isize,
    visible: bool,
}

pub struct Window {
    pub hwnd: Cell<HWND>,
    pub kind: WindowKind,
    shared: Rc<Shared>,
    surface: RefCell<Option<Surface>>,
    dpi: Cell<u32>,
    ix: RefCell<Interaction>,
    widgets: RefCell<Vec<Widget>>,
    edits: RefCell<Vec<EditHandle>>,
    adapter: RefCell<Option<accesskit_windows::Adapter>>,
    actions: Arc<Mutex<Vec<ActionRequest>>>,
    tracking: Cell<bool>,
    /// The host's cursor shape as a Win32 cursor, shown over the video while the viewer controls.
    remote_cursor: crate::cursor::CursorCache,
    scroll_max: Cell<f32>,
    list_area: Cell<Option<Rect>>,
    video: Cell<Option<Rect>>,
    /// The viewer pipeline was told this window exists (and with which video rectangle and DPI).
    announced: Cell<Option<(swapdesk_platform::Rect, u32)>>,
    /// Mouse buttons (bit per `PointerButton`) whose press went to the pipeline and has not been released.
    held: Cell<u8>,
    edit_font: Cell<Option<windows::Win32::Graphics::Gdi::HFONT>>,
    edit_brush: Cell<Option<(bool, HBRUSH)>>,
    /// A modal effect (menu, file dialog) to run after the message that asked for it has finished.
    pending_effect: RefCell<Option<Effect>>,
    /// The text of the address field while it waits for 150 ms of quiet (R-UI-4).
    address_wait: RefCell<Debouncer>,
    /// Full screen (D30): what leaving puts back; `None` while the window has its frame.
    fullscreen: RefCell<Option<crate::fullscreen::Saved>>,
    /// The high half of a surrogate pair typed through the IME, waiting for its low half.
    high_surrogate: Cell<Option<u16>>,
}

struct Bridge {
    actions: Arc<Mutex<Vec<ActionRequest>>>,
    hwnd: isize,
}

impl ActionHandler for Bridge {
    fn do_action(&mut self, request: ActionRequest) {
        if let Ok(mut q) = self.actions.lock() {
            q.push(request);
        }
        // SAFETY: posting to a window handle from any thread is allowed.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut c_void)), WM_ACCESS, WPARAM(0), LPARAM(0));
        }
    }
}

struct Initial<'a> {
    window: &'a Window,
}

impl ActivationHandler for Initial<'_> {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        Some(self.window.tree())
    }
}

fn register_class() -> Result<()> {
    // SAFETY: standard window class registration, once per process (a second call fails harmlessly).
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&class);
    }
    Ok(())
}

impl Window {
    /// Creates and shows the window of `kind`.
    pub fn create(shared: &Rc<Shared>, kind: WindowKind) -> Result<Rc<Window>> {
        register_class()?;
        let window = Rc::new(Window {
            hwnd: Cell::new(HWND::default()),
            kind,
            shared: Rc::clone(shared),
            surface: RefCell::new(None),
            dpi: Cell::new(96),
            ix: RefCell::new(Interaction::default()),
            widgets: RefCell::new(Vec::new()),
            edits: RefCell::new(Vec::new()),
            adapter: RefCell::new(None),
            actions: Arc::new(Mutex::new(Vec::new())),
            tracking: Cell::new(false),
            remote_cursor: crate::cursor::CursorCache::default(),
            scroll_max: Cell::new(0.0),
            list_area: Cell::new(None),
            video: Cell::new(None),
            announced: Cell::new(None),
            held: Cell::new(0),
            edit_font: Cell::new(None),
            edit_brush: Cell::new(None),
            pending_effect: RefCell::new(None),
            address_wait: RefCell::new(Debouncer::new()),
            fullscreen: RefCell::new(None),
            high_surrogate: Cell::new(None),
        });
        let title = a11y::window_title(&shared.state.borrow(), kind);
        let (style, ex) = match kind {
            WindowKind::Banner => {
                (WS_POPUP, WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)
            }
            _ => (WS_OVERLAPPEDWINDOW, WS_EX_NOREDIRECTIONBITMAP),
        };
        let raw = Rc::into_raw(Rc::clone(&window));
        // SAFETY: the pointer is reclaimed in WM_NCDESTROY; creation failure reclaims it here.
        let created = unsafe {
            let instance = GetModuleHandleW(None)?;
            CreateWindowExW(
                ex,
                CLASS_NAME,
                &windows::core::HSTRING::from(title),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                480,
                None,
                None,
                Some(instance.into()),
                Some(raw.cast::<c_void>()),
            )
        };
        match created {
            Ok(h) => {
                window.hwnd.set(h);
                if kind == WindowKind::Banner {
                    // The banner is for the person at this machine: kept out of the screen capture, so the viewer
                    // never sees its own "controla este computador" bar (Windows 10 2004 and later; older systems
                    // fail the call and keep showing it, which is harmless).
                    // SAFETY: our own window, just created.
                    let _ = unsafe { SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE) };
                }
                window.place_and_show();
                Ok(window)
            }
            Err(e) => {
                // SAFETY: WM_NCDESTROY never ran for a window that was not created.
                unsafe { drop(Rc::from_raw(raw)) };
                Err(e)
            }
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd.get()
    }

    /// The control that has the keyboard focus, if any.
    pub fn focused_control(&self) -> Option<ControlId> {
        self.ix.borrow().focus
    }

    pub fn scale(&self) -> f32 {
        self.dpi.get() as f32 / 96.0
    }

    /// The raw pointers a renderer attaches its video visual with (stage 15).
    pub fn native_surface(&self) -> Option<swapdesk_platform::NativeSurface> {
        self.surface.borrow().as_ref().map(|s| s.native(&self.shared.gfx()))
    }

    /// The video rectangle in window pixels, as drawn (viewer only).
    pub fn video_area(&self) -> Option<Rect> {
        let s = self.scale();
        self.video.get().map(|r| Rect::new(r.x * s, r.y * s, r.w * s, r.h * s))
    }

    /// Sizes the window so the client area is the design size at the monitor's DPI, centres it on the work area
    /// (shrinking it if the screen is smaller) and shows it.
    fn place_and_show(&self) {
        let hwnd = self.hwnd();
        // SAFETY: plain Win32 calls on our own window.
        unsafe {
            let dpi = GetDpiForWindow(hwnd);
            self.dpi.set(dpi.max(96));
            let scale = dpi as f32 / 96.0;
            let mut work = RECT::default();
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
                work = info.rcWork;
            }
            let confirm = self.shared.state.borrow().elevated_confirm().is_some();
            let (dw, dh) = match self.kind {
                WindowKind::Banner => swapdesk_paint::banner_size(),
                // The elevated instance's window is the confirmation card alone.
                _ if confirm => swapdesk_paint::confirm_size(),
                _ => (layout::DESIGN_W, layout::DESIGN_H),
            };
            let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
            let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
            let mut frame =
                RECT { left: 0, top: 0, right: (dw * scale).round() as i32, bottom: (dh * scale).round() as i32 };
            let _ = AdjustWindowRectExForDpi(&mut frame, style, false, ex, dpi);
            let (mut w, mut h) = (frame.right - frame.left, frame.bottom - frame.top);
            let (avail_w, avail_h) = (work.right - work.left, work.bottom - work.top);
            if avail_w > 0 && avail_h > 0 {
                w = w.min(avail_w);
                h = h.min(avail_h);
                let (x, y) = match self.kind {
                    WindowKind::Banner => (work.left + (avail_w - w) / 2, work.top + (16.0 * scale) as i32),
                    WindowKind::Viewer => (
                        work.left + (avail_w - w) / 2 + (24.0 * scale) as i32,
                        work.top + (avail_h - h) / 2 + (24.0 * scale) as i32,
                    ),
                    WindowKind::Main => (work.left + (avail_w - w) / 2, work.top + (avail_h - h) / 2),
                };
                let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
            let _ = ShowWindow(hwnd, if self.kind == WindowKind::Banner { SW_SHOWNOACTIVATE } else { SW_SHOW });
            if confirm {
                // Started from the UAC prompt: the person must see it in front, not find it behind the app.
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    // ----- the viewer pipeline (stage 18g) -----

    /// The pipeline's hooks, for the viewer window only.
    fn host(&self) -> Option<&Arc<dyn ViewerHost>> {
        if self.kind == WindowKind::Viewer { self.shared.host.as_ref() } else { None }
    }

    /// The video rectangle in whole window pixels.
    fn video_px(&self) -> Option<swapdesk_platform::Rect> {
        self.video_area().map(|r| swapdesk_platform::Rect {
            x: r.x.round() as i32,
            y: r.y.round() as i32,
            width: r.w.round().max(1.0) as u32,
            height: r.h.round().max(1.0) as u32,
        })
    }

    /// After a frame was laid out: tells the pipeline the window exists, or that its video rectangle or DPI changed
    /// (this is where `WM_SIZE` and `WM_DPICHANGED` reach the renderer: their invalidation repaints, the layout
    /// gives the rectangle).
    fn sync_viewer_host(&self) {
        let Some(host) = self.host() else { return };
        let (Some(area), dpi) = (self.video_px(), self.dpi.get()) else { return };
        let scale = self.scale();
        match self.announced.get() {
            None => {
                if let Some(surface) = self.native_surface() {
                    host.opened(surface, area, scale);
                    self.announced.set(Some((area, dpi)));
                }
            }
            Some(last) if last != (area, dpi) => {
                host.resized(area, scale);
                self.announced.set(Some((area, dpi)));
            }
            Some(_) => {}
        }
    }

    /// Offers a mouse message to the pipeline. True when it took it.
    fn forward_mouse(&self, kind: MouseKind, x: i32, y: i32, hit: Option<ControlId>) -> bool {
        let (Some(host), Some(area)) = (self.host(), self.video_px()) else { return false };
        let held = self.held.get() != 0;
        if hit.is_some() && !held {
            return false; // the toolbar owns the pointer
        }
        let inside = x >= area.x && y >= area.y && x < area.x + area.width as i32 && y < area.y + area.height as i32;
        if !inside && !held {
            return false;
        }
        host.mouse(MouseMsg { kind, x, y, inside })
    }

    /// A button press or release over the video.
    fn forward_button(&self, button: PointerButton, down: bool, lparam: LPARAM) -> bool {
        let (x, y) = (get_x(lparam.0), get_y(lparam.0));
        let bit = 1u8 << (button as u8);
        if !down && self.held.get() & bit == 0 {
            return false;
        }
        let hit = self.hit_test(x, y);
        if !self.forward_mouse(MouseKind::Button { button, down }, x, y, hit) {
            return false;
        }
        let was = self.held.get();
        let now = if down { was | bit } else { was & !bit };
        self.held.set(now);
        // SAFETY: our own window takes or gives up the pointer capture.
        unsafe {
            if was == 0 && now != 0 {
                SetCapture(self.hwnd());
            } else if was != 0 && now == 0 {
                let _ = ReleaseCapture();
            }
        }
        true
    }

    /// Our window lost the mouse capture to someone else (a menu, a dialog, another window, `WM_CANCELMODE`) while a
    /// button may be down: no button-up will ever arrive, so the press is undone here. The UI's pressed control is
    /// dropped (no activation), and every button the viewer believes is down gets its button-up, so none stays
    /// stuck on the remote host.
    fn capture_lost(&self) {
        let held = self.held.get();
        let was_pressed = self.ix.borrow_mut().pressed.take().is_some();
        if held != 0 {
            let mut p = windows::Win32::Foundation::POINT::default();
            // SAFETY: our window; the cursor position is read and converted to client pixels.
            unsafe {
                let _ = GetCursorPos(&mut p);
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd(), &mut p);
            }
            for button in held_buttons(held) {
                // `held` is still non-zero, so the pipeline gets the release wherever the pointer went.
                self.forward_mouse(MouseKind::Button { button, down: false }, p.x, p.y, None);
            }
            self.held.set(0);
        }
        if held != 0 || was_pressed {
            self.invalidate();
        }
    }

    fn forward_wheel(&self, horizontal: bool, wparam: WPARAM, lparam: LPARAM) -> bool {
        // The wheel messages carry screen coordinates.
        let mut p = windows::Win32::Foundation::POINT { x: get_x(lparam.0), y: get_y(lparam.0) };
        // SAFETY: our window.
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd(), &mut p);
        }
        let delta = hiword(wparam.0 as isize) as i16;
        let kind =
            if horizontal { MouseKind::Wheel { dx: delta, dy: 0 } } else { MouseKind::Wheel { dx: 0, dy: delta } };
        let hit = self.hit_test(p.x, p.y);
        self.forward_mouse(kind, p.x, p.y, hit)
    }

    /// Full screen follows the state (D30): the viewer window covers the chosen monitor with no frame, or has its frame
    /// back. A change of the chosen monitor while already full screen applies the next time.
    pub fn sync_fullscreen(&self, on: bool, monitor: Option<u8>) {
        if self.kind != WindowKind::Viewer {
            return;
        }
        let mut saved = self.fullscreen.borrow_mut();
        match (on, saved.as_ref()) {
            (true, None) => {
                if let Some(target) = crate::fullscreen::target_rect(self.hwnd(), monitor) {
                    *saved = Some(crate::fullscreen::enter(self.hwnd(), target));
                }
            }
            (false, Some(s)) => {
                crate::fullscreen::leave(self.hwnd(), s);
                *saved = None;
            }
            _ => {}
        }
    }

    /// Text the IME composed: the key-downs of an IME arrive as `VK_PROCESSKEY` and carry nothing, the result comes as
    /// `WM_CHAR`s that nothing else reads in the viewer. They go to the pipeline as a text-only [`KeyMsg`]
    /// (`vk == 0`), which sends a text record. `Some` when the unit was taken.
    fn forward_text(&self, unit: u16) -> Option<LRESULT> {
        let host = self.host()?;
        if self.ix.borrow().focus.is_some() || !host.captures_keyboard() {
            return None;
        }
        let pair = match (self.high_surrogate.take(), unit) {
            (Some(high), low) if (0xDC00..0xE000).contains(&low) => vec![high, low],
            (_, high) if (0xD800..0xDC00).contains(&high) => {
                self.high_surrogate.set(Some(high));
                return Some(LRESULT(0));
            }
            (_, u) => vec![u],
        };
        let text = String::from_utf16(&pair).ok().filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
        if let Some(text) = text {
            let key = KeyMsg { vk: 0, scan: 0, extended: false, down: true, text: Some(text), dead: false, locks: 0 };
            host.key(&key);
        }
        Some(LRESULT(0))
    }

    /// Offers a key message to the pipeline. `Some` when it takes the key away from the UI.
    fn forward_key(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let host = self.host()?;
        if self.ix.borrow().focus.is_some() || !host.captures_keyboard() {
            return None; // a toolbar control has the keyboard focus, or nobody wants the keys
        }
        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let sys = matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP);
        let (text, dead) = if down { self.take_chars() } else { (None, false) };
        let key = KeyMsg {
            vk: wparam.0 as u32,
            scan: (lparam.0 as u32 >> 16) & 0xFF,
            extended: (lparam.0 as u32 >> 24) & 1 != 0,
            down,
            text,
            dead,
            locks: lock_state(),
        };
        if !host.key(&key) {
            return None;
        }
        // System keys still reach the default procedure (Alt+F4, Alt+Space act on this window: that is what leaving
        // "Capturar atalhos do sistema" off means).
        if sys { None } else { Some(LRESULT(0)) }
    }

    /// IME text: the shell has no IME UI and handles no `WM_IME_*` message (`handle` returns `None` for them, so they
    /// reach `DefWindowProc`, which shows the system composition window and turns the finished result, from
    /// `WM_IME_COMPOSITION` / `WM_IME_CHAR`, into ordinary `WM_CHAR`s). The native EDIT fields get their text the same
    /// way. In the viewer, `WM_CHAR` is the only path that carries text, and it is read here.
    ///
    /// The text the key press that is being handled produced: the `WM_CHAR`s that `TranslateMessage` queued for it,
    /// removed (the viewer has no text input), and whether the key was a dead key.
    fn take_chars(&self) -> (Option<String>, bool) {
        let mut units: Vec<u16> = Vec::new();
        let mut dead = false;
        let mut m = MSG::default();
        // SAFETY: peeks and removes character messages of our own window.
        unsafe {
            while units.len() < 4
                && PeekMessageW(&mut m, Some(self.hwnd()), WM_CHAR, WM_SYSDEADCHAR, PM_REMOVE).as_bool()
            {
                match m.message {
                    WM_DEADCHAR | WM_SYSDEADCHAR => dead = true,
                    _ => units.push(m.wParam.0 as u16),
                }
            }
        }
        let text = String::from_utf16(&units).ok().filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
        (text, dead)
    }

    /// Lets go of the drawing surface: the viewer's pipeline detaches from it first, and the next paint makes a new
    /// one (and announces it to the pipeline again).
    pub fn drop_surface(&self) {
        if self.announced.take().is_some()
            && let Some(host) = self.host()
        {
            host.closed();
        }
        *self.surface.borrow_mut() = None;
    }

    pub fn invalidate(&self) {
        // SAFETY: our window; repaints the whole client area on the next WM_PAINT.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd()), None, false);
        }
    }

    /// A hover, press or focus change touches only the controls that lost or gained the state: their rectangles (as
    /// last drawn, grown by the focus ring and shadow margin and scaled by the DPI) are invalidated instead of the
    /// whole window. A control with no drawn rectangle (the layout is stale) falls back to the whole window. Layout
    /// changes keep calling [`Window::invalidate`].
    fn invalidate_controls(&self, ids: &[Option<ControlId>]) {
        let s = self.scale();
        let mut rects: Vec<RECT> = Vec::new();
        {
            let widgets = self.widgets.borrow();
            for id in ids.iter().flatten() {
                let Some(w) = widgets.iter().find(|w| w.id == *id) else {
                    drop(widgets);
                    return self.invalidate();
                };
                let r = w.rect;
                rects.push(RECT {
                    left: ((r.x - CONTROL_MARGIN) * s).floor() as i32,
                    top: ((r.y - CONTROL_MARGIN) * s).floor() as i32,
                    right: ((r.right() + CONTROL_MARGIN) * s).ceil() as i32,
                    bottom: ((r.bottom() + CONTROL_MARGIN) * s).ceil() as i32,
                });
            }
        }
        for rc in &rects {
            // SAFETY: our window; the rectangle is a local, read during the call.
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd()), Some(rc), false);
            }
        }
    }

    /// As [`Window::invalidate_controls`], but a press or release that touches no control (the viewer's video area, the
    /// background) still repaints the whole window, as before.
    fn invalidate_changed(&self, ids: &[Option<ControlId>]) {
        if ids.iter().all(Option::is_none) {
            self.invalidate();
        } else {
            self.invalidate_controls(ids);
        }
    }

    fn client_size(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        // SAFETY: our window.
        unsafe {
            let _ = GetClientRect(self.hwnd(), &mut rc);
        }
        ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)
    }

    fn set_title_bar_theme(&self) {
        let dark: i32 = self.shared.state.borrow().dark().into();
        // SAFETY: the attributes take a BOOL-sized / DWORD-sized value that outlives the call.
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.hwnd(),
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                std::ptr::from_ref(&dark).cast(),
                size_of::<i32>() as u32,
            );
            if self.kind == WindowKind::Banner {
                let round: i32 = 2;
                let _ = DwmSetWindowAttribute(
                    self.hwnd(),
                    DWMWA_WINDOW_CORNER_PREFERENCE,
                    std::ptr::from_ref(&round).cast(),
                    size_of::<i32>() as u32,
                );
            }
        }
    }

    pub fn theme_changed(&self) {
        self.set_title_bar_theme();
        self.invalidate();
    }

    // ----- hit testing, focus -----

    fn hit_test(&self, x: i32, y: i32) -> Option<ControlId> {
        let s = self.scale();
        swapdesk_paint::input::hit_test(&self.widgets.borrow(), x as f32 / s, y as f32 / s)
    }

    /// The focusable controls that were drawn, in Tab order.
    fn tab_order(&self) -> Vec<ControlId> {
        swapdesk_paint::input::tab_order(&self.shared.state.borrow(), self.kind, &self.widgets.borrow())
    }

    fn move_focus(&self, backwards: bool) {
        let current = self.ix.borrow().focus;
        if let Some(next) = swapdesk_paint::input::next_focus(&self.tab_order(), current, backwards) {
            self.focus_control(next, true);
        }
    }

    pub fn focus_control(&self, id: ControlId, visible: bool) {
        #[cfg(test)]
        {
            *LAST_FOCUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
        }
        {
            let mut ix = self.ix.borrow_mut();
            ix.focus = Some(id);
            ix.focus_visible = visible;
        }
        if let ControlId::Edit(e) = id
            && let Some(h) = self.edits.borrow().iter().find(|h| h.id == e).map(|h| h.hwnd)
        {
            // SAFETY: our child window.
            unsafe {
                let _ = SetFocus(Some(h));
            }
        } else {
            // SAFETY: our own window takes the keyboard focus back from a native input.
            unsafe {
                let _ = SetFocus(Some(self.hwnd()));
            }
        }
        self.invalidate();
        self.update_access();
    }

    // ----- accessibility -----

    fn bounds_of(&self, id: ControlId) -> Option<(f64, f64, f64, f64)> {
        let s = f64::from(self.scale());
        self.widgets.borrow().iter().find(|w| w.id == id).map(|w| {
            (
                f64::from(w.rect.x) * s,
                f64::from(w.rect.y) * s,
                f64::from(w.rect.right()) * s,
                f64::from(w.rect.bottom()) * s,
            )
        })
    }

    fn tree(&self) -> TreeUpdate {
        let focus = self.ix.borrow().focus;
        let mut tree = a11y::build_tree(&self.shared.state.borrow(), self.kind, &|id| self.bounds_of(id), focus);
        crate::live_region::name_live_regions(&mut tree);
        tree
    }

    /// Hands the current tree to UI Automation (if a client asked for it).
    pub fn update_access(&self) {
        let events = {
            let mut adapter = self.adapter.borrow_mut();
            adapter.as_mut().and_then(|a| a.update_if_active(|| self.tree()))
        };
        if let Some(e) = events {
            e.raise();
        }
    }

    fn handle_access_actions(&self) {
        let requests: Vec<ActionRequest> = self.actions.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for r in requests {
            let target = {
                let st = self.shared.state.borrow();
                a11y::control_of(&st, self.kind, r.target_node)
            };
            let Some(id) = target else { continue };
            match r.action {
                Action::Click => self.shared.activate(id, self),
                Action::Focus => self.focus_control(id, true),
                _ => {}
            }
        }
    }

    // ----- native text inputs -----

    fn ensure_edit_font(&self) -> windows::Win32::Graphics::Gdi::HFONT {
        if let Some(f) = self.edit_font.get() {
            return f;
        }
        let f = edit_font(self.dpi.get());
        self.edit_font.set(Some(f));
        f
    }

    /// Creates, moves, fills and hides the native inputs to match what the frame drew.
    fn place_edits(&self, slots: &[EditSlot]) {
        let s = self.scale();
        let font = self.ensure_edit_font();
        let mut handles = self.edits.borrow_mut();
        for slot in slots {
            let line = (slot.rect.h.min(22.0) * s).round() as i32;
            let (x, y, w) =
                ((slot.rect.x * s).round() as i32, (slot.rect.y * s).round() as i32, (slot.rect.w * s).round() as i32);
            let y = y + ((slot.rect.h * s).round() as i32 - line) / 2;
            let existing = handles.iter().find(|h| h.id == slot.id).map(|h| h.hwnd);
            let mut created_now = false;
            let hwnd = match existing {
                Some(h) => h,
                None => {
                    created_now = true;
                    // SAFETY: a child EDIT of our window; it is destroyed with it.
                    let created = unsafe {
                        let instance = GetModuleHandleW(None).ok();
                        CreateWindowExW(
                            // Layered: the top-level window has no redirection bitmap (DirectComposition), and a plain
                            // GDI child has nothing to draw into there.
                            WS_EX_LAYERED,
                            w!("EDIT"),
                            w!(""),
                            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL_STYLE),
                            x,
                            y,
                            w,
                            line,
                            Some(self.hwnd()),
                            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(edit_code(slot.id) as *mut c_void)),
                            instance.map(Into::into),
                            None,
                        )
                    };
                    let Ok(h) = created else { continue };
                    // SAFETY: our own child; fully opaque, the layered style only gives it a surface.
                    unsafe {
                        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
                    }
                    // SAFETY: subclass our own child: keys the parent must see (Enter, Tab, Escape) go to it first.
                    unsafe {
                        let original = SetWindowLongPtrW(h, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
                        let _ = EDIT_ORIGINAL_PROC.compare_exchange(0, original, Ordering::SeqCst, Ordering::SeqCst);
                        let cue = windows::core::HSTRING::from(slot.placeholder.as_str());
                        if !slot.placeholder.is_empty() {
                            SendMessageW(h, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));
                        }
                        let _ = SetWindowTextW(h, &windows::core::HSTRING::from(slot.value.as_str()));
                    }
                    handles.push(EditHandle { id: slot.id, hwnd: h, rect: (x, y, w, line), font: 0, visible: true });
                    h
                }
            };
            let handle = handles.iter_mut().find(|h| h.id == slot.id).expect("the handle was just found or pushed");
            // The rename field opens prefilled, with its text selected and the keyboard in it.
            let opening = slot.id == EditId::Rename && (created_now || !handle.visible);
            // SAFETY: our child windows.
            unsafe {
                if handle.font != font.0 as isize {
                    SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
                    handle.font = font.0 as isize;
                }
                if handle.rect != (x, y, w, line) || !handle.visible {
                    let _ = SetWindowPos(hwnd, None, x, y, w, line, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
                    handle.rect = (x, y, w, line);
                    handle.visible = true;
                }
                if opening {
                    let _ = SetWindowTextW(hwnd, &windows::core::HSTRING::from(slot.value.as_str()));
                    let _ = SetFocus(Some(hwnd));
                    SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                }
                // The rename field keeps what was typed (a click on "Salvar" takes the focus before it acts).
                if slot.id != EditId::Rename
                    && GetFocus() != hwnd
                    && !slot.value.is_empty()
                    && edit_text(hwnd) != slot.value
                {
                    let _ = SetWindowTextW(hwnd, &windows::core::HSTRING::from(slot.value.as_str()));
                }
            }
        }
        for h in handles.iter_mut().filter(|h| h.visible && !slots.iter().any(|s| s.id == h.id)) {
            // SAFETY: our child window.
            unsafe {
                let _ = ShowWindow(h.hwnd, SW_HIDE);
            }
            h.visible = false;
        }
    }

    fn edit_hwnd(&self, id: EditId) -> Option<HWND> {
        self.edits.borrow().iter().find(|h| h.id == id).map(|h| h.hwnd)
    }

    /// The text of a native input (empty when it does not exist).
    pub fn edit_value(&self, id: EditId) -> String {
        self.edit_hwnd(id).map(edit_text).unwrap_or_default()
    }

    pub fn clear_edit(&self, id: EditId) {
        if let Some(h) = self.edit_hwnd(id) {
            // SAFETY: our child window.
            unsafe {
                let _ = SetWindowTextW(h, w!(""));
            }
        }
    }

    pub fn set_edit_text(&self, id: EditId, text: &str) {
        if let Some(h) = self.edit_hwnd(id) {
            // SAFETY: our child window.
            unsafe {
                let _ = SetWindowTextW(h, &windows::core::HSTRING::from(text));
            }
        }
    }

    fn on_edit_event(&self, code: usize, event: isize) {
        let Some(id) = edit_from_code(code) else { return };
        match event {
            1 => {
                // Enter: commit now (the chat line also clears); an address waiting for its 150 ms goes with it.
                if id == EditId::Address {
                    self.address_wait.borrow_mut().flush();
                    // SAFETY: our own window's timer.
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd()), TIMER_ADDRESS);
                    }
                }
                let text = self.edit_value(id);
                self.shared.submit_edit(id, text, self);
                if id == EditId::Chat {
                    self.clear_edit(id);
                }
                // The rename field keeps the focus: a refused name is corrected in place.
                if id != EditId::Rename {
                    // SAFETY: our own window takes the focus back.
                    unsafe {
                        let _ = SetFocus(Some(self.hwnd()));
                    }
                }
            }
            2 => self.move_focus(unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0),
            3 => {
                if id == EditId::Rename {
                    self.shared.activate(ControlId::RenameCancel, self);
                }
                if id == EditId::Name
                    && let Some(n) = self.shared.state.borrow().settings().map(|s| s.name.clone())
                {
                    self.set_edit_text(id, &n);
                }
                // SAFETY: our own window takes the focus back.
                unsafe {
                    let _ = SetFocus(Some(self.hwnd()));
                }
            }
            4 if id == EditId::Name => {
                let text = self.edit_value(id);
                self.shared.submit_edit(id, text, self);
            }
            _ => {}
        }
    }

    // ----- painting -----

    fn ensure_surface(&self) -> Result<()> {
        if self.surface.borrow().is_some() {
            return Ok(());
        }
        let (w, h) = self.client_size();
        let s = Surface::new(&self.shared.gfx(), self.hwnd(), w, h, self.kind == WindowKind::Viewer)?;
        *self.surface.borrow_mut() = Some(s);
        Ok(())
    }

    fn paint(&self) {
        #[cfg(test)]
        PAINT_COUNT.fetch_add(1, Ordering::SeqCst);
        let _g = SlowGuard::ui("paint");
        if self.ensure_surface().is_err() {
            return;
        }
        let gfx = self.shared.gfx();
        // The text formats follow the interface language (their DirectWrite locale); a change drops the cached ones.
        gfx.fonts.set_language(self.shared.state.borrow().language());
        let dpi = self.dpi.get() as f32;
        let scale = dpi / 96.0;
        let out;
        let terminal_commands;
        {
            let surface_ref = self.surface.borrow();
            let Some(surface) = surface_ref.as_ref() else { return };
            let Ok(dc) = surface.begin(dpi) else {
                drop(surface_ref);
                *self.surface.borrow_mut() = None;
                return;
            };
            // SAFETY: between BeginDraw and EndDraw.
            let size = unsafe { dc.GetSize() };
            let factory = gfx.factory();
            let Ok(painter) = D2dPainter::new(dc, &factory, &gfx.strokes, &gfx.fonts, scale) else {
                let _ = surface.end();
                return;
            };
            let st = self.shared.state.borrow();
            let ix = self.ix.borrow();
            let frame = Frame::new(&painter, &st, &ix, (size.width, size.height), false);
            out = swapdesk_paint::paint(frame, self.kind);
            let grid_focused = ix.focus == Some(ControlId::TermGrid);
            drop(ix);
            drop(st);
            // The drawer's terminal grid, over the frame (its renderer is made the first time the panel shows it).
            terminal_commands = self.shared.terminal.borrow_mut().paint(dc, out.terminal, scale, grid_focused);
            if let Err(e) = surface.end()
                && e.code() == D2DERR_RECREATE_TARGET
            {
                drop(surface_ref);
                if gfx.device_removed() {
                    // The device itself is gone: every window and the viewer move to a new one (they repaint when
                    // it is made; while it cannot be, the next wake or message tries again, never a busy loop).
                    let _ = self.shared.recover_device();
                } else {
                    *self.surface.borrow_mut() = None;
                    self.invalidate();
                }
                return;
            }
        }
        // A new grid size is a command; it goes out now that nothing of this frame is borrowed.
        self.shared.send_commands(terminal_commands);
        self.scroll_max.set(out.scroll_max);
        self.list_area.set(out.list_area);
        self.video.set(out.video);
        self.sync_viewer_host();
        // The wheel can leave the list scrolled past a shorter list; another scrolling content starts at its top.
        {
            let mut ix = self.ix.borrow_mut();
            if ix.scroll_key != out.scroll_key {
                ix.scroll_key = out.scroll_key;
                ix.scroll = 0.0;
            }
            ix.scroll = ix.scroll.clamp(0.0, out.scroll_max);
        }
        *self.widgets.borrow_mut() = out.widgets;
        self.place_edits(&out.edits);
        self.update_access();
    }

    // ----- messages -----

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let hwnd = self.hwnd();
        // The drawer's terminal takes the keys while its grid has the focus, and the mouse over its grid.
        if let Some(r) = crate::terminal::message(&self.shared, self, msg, wparam, lparam) {
            return Some(r);
        }
        if matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
            && let Some(r) = self.forward_key(msg, wparam, lparam)
        {
            return Some(r);
        }
        if msg == WM_CHAR
            && self.kind == WindowKind::Viewer
            && let Some(r) = self.forward_text(wparam.0 as u16)
        {
            return Some(r);
        }
        match msg {
            WM_SYSCOMMAND
                if (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU
                    && lparam.0 == 0
                    && self.host().is_some_and(|h| h.captures_keyboard()) =>
            {
                // A bare Alt tap would open the system menu and eat the next keys: they belong to the remote.
                Some(LRESULT(0))
            }
            WM_CREATE => {
                // SAFETY: our own window.
                self.dpi.set(unsafe { GetDpiForWindow(hwnd) }.max(96));
                self.set_title_bar_theme();
                let bridge = Bridge { actions: Arc::clone(&self.actions), hwnd: hwnd.0 as isize };
                *self.adapter.borrow_mut() =
                    Some(accesskit_windows::Adapter::new(accesskit_windows::HWND(hwnd.0), false, bridge));
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                // SAFETY: paired Begin/EndPaint on our window (validates the update region).
                unsafe {
                    windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut ps);
                    self.paint();
                    let _ = windows::Win32::Graphics::Gdi::EndPaint(hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                if wparam.0 as u32 != SIZE_MINIMIZED {
                    let (w, h) = (u32::from(loword(lparam.0)), u32::from(hiword(lparam.0)));
                    if let Some(s) = self.surface.borrow_mut().as_mut()
                        && s.resize(w, h).is_err()
                    {
                        self.surface.replace(None);
                    }
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.dpi.set(u32::from(loword(wparam.0 as isize)));
                // The cached text layouts were made for the old scale.
                self.shared.gfx().fonts.clear_layouts();
                if let Some(f) = self.edit_font.take() {
                    // SAFETY: the font is ours and no control uses it after the next WM_SETFONT.
                    unsafe {
                        let _ = DeleteObject(f.into());
                    }
                }
                // SAFETY: lParam points at the suggested rectangle for the new DPI.
                unsafe {
                    let r = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                // The banner and the confirmation window keep the size they were given.
                if self.kind == WindowKind::Banner || self.shared.state.borrow().elevated_confirm().is_some() {
                    return None;
                }
                let dpi = self.dpi.get();
                let scale = dpi as f32 / 96.0;
                let mut r = RECT {
                    left: 0,
                    top: 0,
                    right: (layout::MIN_W * scale).round() as i32,
                    bottom: (layout::MIN_H * scale).round() as i32,
                };
                // SAFETY: lParam points at a MINMAXINFO for this message.
                unsafe {
                    let _ =
                        AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, false, WS_EX_NOREDIRECTIONBITMAP, dpi);
                    let info = &mut *(lparam.0 as *mut MINMAXINFO);
                    info.ptMinTrackSize.x = r.right - r.left;
                    info.ptMinTrackSize.y = r.bottom - r.top;
                }
                Some(LRESULT(0))
            }
            WM_SETTINGCHANGE => {
                // The app theme switch arrives as "ImmersiveColorSet".
                // SAFETY: for this message lParam is null or a NUL-terminated wide string.
                let is_theme = lparam.0 != 0
                    && unsafe { PCWSTR(lparam.0 as *const u16).to_string().is_ok_and(|s| s == "ImmersiveColorSet") };
                // Every top-level window gets the broadcast; the handler updates all the windows (palette, title bar
                // and repaint), whichever kind received it first.
                if is_theme {
                    self.shared.system_theme_changed();
                }
                None
            }
            WM_MOUSEMOVE => {
                let (x, y) = (get_x(lparam.0), get_y(lparam.0));
                if !self.tracking.get() {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: valid struct for our window.
                    self.tracking.set(unsafe { TrackMouseEvent(&mut tme) }.is_ok());
                }
                let hit = self.hit_test(x, y);
                let previous = std::mem::replace(&mut self.ix.borrow_mut().hover, hit);
                if previous != hit {
                    self.invalidate_controls(&[previous, hit]);
                }
                self.forward_mouse(MouseKind::Move, x, y, hit);
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                // lParam is the window that gets the capture; our own SetCapture calls (same window) change nothing.
                if lparam.0 != hwnd.0 as isize {
                    self.capture_lost();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking.set(false);
                let left = self.ix.borrow_mut().hover.take();
                if left.is_some() {
                    self.invalidate_controls(&[left]);
                }
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                if u32::from(loword(lparam.0)) != HTCLIENT {
                    return None;
                }
                let hover = self.ix.borrow().hover;
                if hover.is_none()
                    && let Some(host) = self.host()
                    && let Some(shape) = host.pointer_shape()
                    && let Some(a) = self.video_px()
                {
                    let mut p = windows::Win32::Foundation::POINT::default();
                    // SAFETY: our window; the cursor position is read and converted to client pixels.
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p);
                    }
                    if p.x >= a.x
                        && p.y >= a.y
                        && p.x < a.x + a.width as i32
                        && p.y < a.y + a.height as i32
                        && let Some(c) = self.remote_cursor.get(&shape)
                    {
                        // SAFETY: a cursor the window's cache owns; it lives until the cache replaces it.
                        unsafe { SetCursor(Some(c)) };
                        return Some(LRESULT(1));
                    }
                }
                let cursor = match hover {
                    Some(ControlId::Edit(_)) => IDC_IBEAM,
                    Some(_) => IDC_HAND,
                    None => IDC_ARROW,
                };
                // SAFETY: stock cursors.
                unsafe {
                    if let Ok(c) = LoadCursorW(None, cursor) {
                        SetCursor(Some(c));
                    }
                }
                Some(LRESULT(1))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = (get_x(lparam.0), get_y(lparam.0));
                let hit = self.hit_test(x, y);
                let before = {
                    let mut ix = self.ix.borrow_mut();
                    let before = [ix.pressed, ix.hover, ix.focus];
                    ix.pressed = hit;
                    ix.hover = hit;
                    ix.focus = hit;
                    ix.focus_visible = false;
                    before
                };
                self.forward_button(PointerButton::Left, true, lparam);
                if let Some(ControlId::Edit(e)) = hit {
                    self.focus_control(ControlId::Edit(e), false);
                } else {
                    // SAFETY: our own window takes the focus from any native input.
                    unsafe {
                        let _ = SetFocus(Some(hwnd));
                        SetCapture(hwnd);
                    }
                }
                self.invalidate_changed(&[before[0], before[1], before[2], hit]);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = (get_x(lparam.0), get_y(lparam.0));
                let pressed = self.ix.borrow_mut().pressed.take();
                let hit = self.hit_test(x, y);
                let was_hover = std::mem::replace(&mut self.ix.borrow_mut().hover, hit);
                self.forward_button(PointerButton::Left, false, lparam);
                if self.held.get() == 0 {
                    // SAFETY: balances SetCapture.
                    let _ = unsafe { ReleaseCapture() };
                }
                self.invalidate_changed(&[pressed, was_hover, hit]);
                if let (Some(p), Some(h)) = (pressed, hit)
                    && p == h
                    && !matches!(p, ControlId::Edit(_))
                {
                    self.shared.activate(p, self);
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if self.forward_wheel(false, wparam, lparam) => Some(LRESULT(0)),
            WM_MOUSEHWHEEL if self.forward_wheel(true, wparam, lparam) => Some(LRESULT(0)),
            WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP if self.host().is_some() => {
                let (button, down) = match msg {
                    WM_RBUTTONDOWN => (PointerButton::Right, true),
                    WM_RBUTTONUP => (PointerButton::Right, false),
                    WM_MBUTTONDOWN => (PointerButton::Middle, true),
                    _ => (PointerButton::Middle, false),
                };
                if down {
                    // SAFETY: our own window takes the keyboard focus, as a left click does.
                    unsafe {
                        let _ = SetFocus(Some(hwnd));
                    }
                    self.ix.borrow_mut().focus = None;
                }
                self.forward_button(button, down, lparam);
                Some(LRESULT(0))
            }
            WM_XBUTTONDOWN | WM_XBUTTONUP if self.host().is_some() => {
                let button = if hiword(wparam.0 as isize) == 1 { PointerButton::Back } else { PointerButton::Forward };
                self.forward_button(button, msg == WM_XBUTTONDOWN, lparam);
                Some(LRESULT(1))
            }
            WM_MOUSEWHEEL => {
                let delta = i32::from(hiword(wparam.0 as isize) as i16);
                let max = self.scroll_max.get();
                if max > 0.0 {
                    let mut ix = self.ix.borrow_mut();
                    ix.scroll = (ix.scroll - delta as f32 / 120.0 * 50.0).clamp(0.0, max);
                    drop(ix);
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let key = wparam.0 as u16;
                if key == VK_TAB.0 {
                    // SAFETY: plain key state read.
                    let backwards = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
                    self.move_focus(backwards);
                    return Some(LRESULT(0));
                }
                if key == VK_RETURN.0 || key == VK_SPACE.0 {
                    let focus = self.ix.borrow().focus;
                    if let Some(id) = focus.filter(|i| !matches!(i, ControlId::Edit(_))) {
                        self.shared.activate(id, self);
                    }
                    return Some(LRESULT(0));
                }
                if key == VK_ESCAPE.0 {
                    if self.kind != WindowKind::Main || self.shared.escape(self) {
                        return Some(LRESULT(0));
                    }
                    return None;
                }
                None
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                let focused = msg == WM_SETFOCUS;
                if let Some(host) = self.host() {
                    host.focus(focused);
                }
                let events = self.adapter.borrow_mut().as_mut().and_then(|a| a.update_window_focus_state(focused));
                if let Some(e) = events {
                    e.raise();
                }
                None
            }
            WM_GETOBJECT => {
                let mut init = Initial { window: self };
                let result = {
                    let mut adapter = self.adapter.borrow_mut();
                    adapter.as_mut().and_then(|a| {
                        a.handle_wm_getobject(
                            accesskit_windows::WPARAM(wparam.0),
                            accesskit_windows::LPARAM(lparam.0),
                            &mut init,
                        )
                    })
                };
                result.map(Into::into)
            }
            WM_CTLCOLOREDIT => {
                let dark = self.shared.state.borrow().dark();
                let pal = palette(dark);
                let brush = match self.edit_brush.get() {
                    Some((d, b)) if d == dark => b,
                    old => {
                        if let Some((_, b)) = old {
                            // SAFETY: the old brush is ours and no longer selected anywhere.
                            unsafe {
                                let _ = DeleteObject(b.into());
                            }
                        }
                        // SAFETY: plain brush creation.
                        let b = unsafe { CreateSolidBrush(colorref(pal.surface.hex())) };
                        self.edit_brush.set(Some((dark, b)));
                        b
                    }
                };
                let hdc = HDC(wparam.0 as *mut c_void);
                // SAFETY: the DC belongs to the control being painted for the duration of this message.
                unsafe {
                    SetTextColor(hdc, colorref(pal.ink.hex()));
                    SetBkColor(hdc, colorref(pal.surface.hex()));
                }
                Some(LRESULT(brush.0 as isize))
            }
            WM_EDIT_EVENT => {
                self.on_edit_event(wparam.0, lparam.0);
                Some(LRESULT(0))
            }
            WM_COMMAND
                if hiword(wparam.0 as isize) == EN_CHANGE
                    && edit_from_code(loword(wparam.0 as isize) as usize) == Some(EditId::Address) =>
            {
                // Typed (or set by us): act 150 ms after the last key. The timer restarts on every key.
                // The text is read from the sender (lParam), never through `self.edits`: `SetWindowTextW` sends
                // EN_CHANGE synchronously while `place_edits` still holds that list mutably borrowed.
                let text = edit_text(HWND(lparam.0 as *mut c_void));
                self.address_wait.borrow_mut().input(&text, 0);
                // SAFETY: our own window's one-shot timer; setting the same id again restarts it.
                unsafe {
                    SetTimer(Some(hwnd), TIMER_ADDRESS, DEBOUNCE_MS as u32, None);
                }
                Some(LRESULT(0))
            }
            WM_ACCESS => {
                self.handle_access_actions();
                Some(LRESULT(0))
            }
            WM_WAKE => {
                self.shared.drain_backend();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_TICK => self.shared.tick(),
                    TIMER_PLACEHOLDER => self.shared.arm_placeholders(),
                    TIMER_BANNER => self.shared.banner_tick(),
                    TIMER_ADDRESS => {
                        // SAFETY: our own window's timer.
                        unsafe {
                            let _ = KillTimer(Some(hwnd), TIMER_ADDRESS);
                        }
                        // The timer restarted with each key, so a firing means 150 ms of quiet.
                        let waiting = self.address_wait.borrow_mut().flush();
                        if let Some(text) = waiting {
                            self.shared.submit_edit(EditId::Address, text, self);
                        }
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                if self.kind == WindowKind::Viewer {
                    // Closing the viewer ends the session; the window goes when the session has ended.
                    self.shared.disconnect_viewer();
                    return Some(LRESULT(0));
                }
                None
            }
            WM_DESTROY => {
                if self.announced.take().is_some()
                    && let Some(host) = self.host()
                {
                    host.closed();
                }
                if self.kind == WindowKind::Main {
                    // SAFETY: ends the message loop.
                    unsafe { PostQuitMessage(0) };
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub fn take_pending_effect(&self) -> Option<Effect> {
        self.pending_effect.borrow_mut().take()
    }

    pub fn set_pending_effect(&self, e: Effect) {
        *self.pending_effect.borrow_mut() = Some(e);
    }

    pub fn screen_point_of(&self, id: ControlId) -> (i32, i32) {
        let s = self.scale();
        let r =
            self.widgets.borrow().iter().find(|w| w.id == id).map(|w| w.rect).unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
        let mut p = windows::Win32::Foundation::POINT { x: (r.x * s) as i32, y: (r.bottom() * s) as i32 };
        // SAFETY: our window.
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.hwnd(), &mut p);
        }
        (p.x, p.y)
    }
}

/// Caps, Num and Scroll Lock toggle state: bits 0, 1 and 2.
fn lock_state() -> u8 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CAPITAL, VK_NUMLOCK, VK_SCROLL};
    // SAFETY: plain key state reads.
    unsafe {
        u8::from(GetKeyState(i32::from(VK_CAPITAL.0)) & 1 != 0)
            | (u8::from(GetKeyState(i32::from(VK_NUMLOCK.0)) & 1 != 0) << 1)
            | (u8::from(GetKeyState(i32::from(VK_SCROLL.0)) & 1 != 0) << 2)
    }
}

fn colorref(rgb: u32) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF))
}

fn edit_code(id: EditId) -> usize {
    match id {
        EditId::Name => 1,
        EditId::Chat => 2,
        EditId::Address => 3,
        EditId::Rename => 4,
    }
}

fn edit_from_code(code: usize) -> Option<EditId> {
    match code {
        1 => Some(EditId::Name),
        2 => Some(EditId::Chat),
        3 => Some(EditId::Address),
        4 => Some(EditId::Rename),
        _ => None,
    }
}

fn edit_text(h: HWND) -> String {
    // SAFETY: our child window; the buffer is sized from GetWindowTextLengthW.
    unsafe {
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// The buttons whose bit is set in a `held` mask (bit = `PointerButton as u8`), in enum order.
fn held_buttons(held: u8) -> Vec<PointerButton> {
    [PointerButton::Left, PointerButton::Right, PointerButton::Middle, PointerButton::Back, PointerButton::Forward]
        .into_iter()
        .filter(|b| held & (1u8 << (*b as u8)) != 0)
        .collect()
}

/// The indices of the windows a system theme change must reach: all of them, whatever their kind.
pub(crate) fn theme_targets(kinds: &[WindowKind]) -> Vec<usize> {
    (0..kinds.len()).collect()
}

/// How far past a control's rectangle a hover, press or focus change can draw (focus ring, shadow), in DIPs.
const CONTROL_MARGIN: f32 = 8.0;

/// Runs a native callback body under `catch_unwind`: a panic must never unwind across the FFI boundary into the
/// system. `Err(())` means it panicked; a line naming the callback (never content) goes to the debugger output, the
/// same channel as the slow-work trace, and the caller falls back to the default behaviour.
///
/// Release builds use `panic = "abort"` (D15): there a panic ends the process before this returns, so the guard
/// protects the unwind builds (tests, dev) and the panic hook's record is kept either way.
fn guard_callback<R>(origin: &'static str, body: impl FnOnce() -> R) -> std::result::Result<R, ()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).map_err(|_| {
        crate::system::trace_line(&format!("swapdesk: panic caught in {origin}"));
    })
}

/// The control the shell focused last (tests read it from another thread).
#[cfg(test)]
pub(crate) static LAST_FOCUS: std::sync::Mutex<Option<ControlId>> = std::sync::Mutex::new(None);

/// Test-only hook point: the next message with this id panics once at the start of its handling (0: none).
#[cfg(test)]
pub(crate) static INJECT_PANIC_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Test-only: how many times a window painted (the live measurement reads it).
#[cfg(test)]
pub(crate) static PAINT_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
fn inject_panic_point(msg: u32) {
    if msg != 0 && INJECT_PANIC_MSG.compare_exchange(msg, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        panic!("injected panic in message {msg:#x}");
    }
}

/// The subclass of the native inputs: Enter, Tab and Escape are the parent's business; losing the focus commits.
pub(crate) unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the original procedure was saved when subclassing; the slot holds a valid WNDPROC (or 0, which
    // `CallWindowProcW` rejects) for the life of the process.
    let original: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(EDIT_ORIGINAL_PROC.load(Ordering::SeqCst)) };
    // `Some(result)`: the subclass handled the message; `None`: it goes on to the control's own procedure.
    let handled = guard_callback("the input subclass procedure", || {
        #[cfg(test)]
        inject_panic_point(msg);
        // SAFETY: called by the system for our EDIT children only.
        unsafe { edit_proc_body(hwnd, msg, wparam) }
    });
    match handled {
        Ok(Some(result)) => result,
        // After a panic the control still gets the message, so typing keeps working.
        // SAFETY: the saved original procedure of the control, called with the arguments we received.
        Ok(None) | Err(()) => unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) },
    }
}

/// # Safety
/// `hwnd` is one of our EDIT children.
unsafe fn edit_proc_body(hwnd: HWND, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    // SAFETY: our child window; the posts go to its parent.
    unsafe {
        let parent = GetParent(hwnd).unwrap_or_default();
        let code = GetDlgCtrlID(hwnd) as usize;
        match msg {
            WM_KEYDOWN => {
                let key = wparam.0 as u16;
                let event = if key == VK_RETURN.0 {
                    Some(1)
                } else if key == VK_TAB.0 {
                    Some(2)
                } else if key == VK_ESCAPE.0 {
                    Some(3)
                } else {
                    None
                };
                if let Some(e) = event {
                    let _ = PostMessageW(Some(parent), WM_EDIT_EVENT, WPARAM(code), LPARAM(e));
                    return Some(LRESULT(0));
                }
            }
            // Swallow the characters of those keys so the control does not beep.
            WM_CHAR if matches!(wparam.0, 0x0D | 0x09 | 0x1B) => return Some(LRESULT(0)),
            WM_KILLFOCUS => {
                let _ = PostMessageW(Some(parent), WM_EDIT_EVENT, WPARAM(code), LPARAM(4));
            }
            _ => {}
        }
        None
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic in any handler is caught here and the message falls to the default procedure: the window keeps
    // answering instead of unwinding into the system.
    // SAFETY: the system calls this procedure with the arguments of the message; `wndproc_body` has no other
    // requirement.
    match guard_callback("the window procedure", || unsafe { wndproc_body(hwnd, msg, wparam, lparam) }) {
        Ok(r) => r,
        // SAFETY: the default procedure of a window of ours, with the arguments we received.
        Err(()) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// # Safety
/// Called only from [`wndproc`], for a window of this class.
unsafe fn wndproc_body(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    #[cfg(test)]
    inject_panic_point(msg);
    // SAFETY: WM_NCCREATE carries the pointer given to CreateWindowExW (an `Rc<Window>` leaked with `into_raw`); it is
    // stored once, read on every later message and released on WM_NCDESTROY, the last message a window receives.
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            let window = create.lpCreateParams as *const Window;
            (*window).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, window as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Window;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let window = Rc::from_raw(ptr);
            window.shared.window_destroyed(&window);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // Keep the window alive for the duration of the message, whatever the handler does.
        Rc::increment_strong_count(ptr);
        let window = Rc::from_raw(ptr);
        let result = {
            let _g = SlowGuard::ui(message_name(msg));
            window.handle(msg, wparam, lparam)
        };
        // Modal things (a menu, a file dialog) run outside the slow-work guard: they wait for a person.
        if let Some(effect) = window.take_pending_effect() {
            window.shared.run_modal(effect, &window);
        }
        match result {
            Some(r) => r,
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// A name for the slow-work trace (names only, never content).
fn message_name(msg: u32) -> &'static str {
    match msg {
        WM_PAINT => "WM_PAINT",
        WM_SIZE => "WM_SIZE",
        WM_MOUSEMOVE => "WM_MOUSEMOVE",
        WM_LBUTTONUP => "WM_LBUTTONUP",
        WM_LBUTTONDOWN => "WM_LBUTTONDOWN",
        WM_KEYDOWN => "WM_KEYDOWN",
        WM_TIMER => "WM_TIMER",
        WM_WAKE => "WM_WAKE",
        WM_GETOBJECT => "WM_GETOBJECT",
        WM_ACCESS => "WM_ACCESS",
        WM_EDIT_EVENT => "WM_EDIT_EVENT",
        WM_DPICHANGED => "WM_DPICHANGED",
        _ => "window message",
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn capture_loss_releases_every_held_button() {
        assert!(held_buttons(0).is_empty());
        assert_eq!(held_buttons(0b00001), vec![PointerButton::Left]);
        assert_eq!(held_buttons(0b10101), vec![PointerButton::Left, PointerButton::Middle, PointerButton::Forward]);
        assert_eq!(held_buttons(0b11111).len(), 5);
    }

    #[test]
    fn theme_change_reaches_every_window_kind() {
        let kinds = [WindowKind::Main, WindowKind::Viewer, WindowKind::Banner];
        assert_eq!(theme_targets(&kinds), vec![0, 1, 2]);
        assert!(theme_targets(&[]).is_empty());
    }
}
