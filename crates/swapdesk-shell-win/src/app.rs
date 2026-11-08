//! The shell's process-level state and message loop: the shared graphics device, the UI state, the backend, the
//! windows that exist, the one-second and 100 ms timers (set only while something needs them) and the glue between
//! user actions and backend commands.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use swapdesk_ui::trace::SlowGuard;
use swapdesk_ui::{Activation, Changed, Command, ControlId, EditId, Effect, SelectId, UiBackend, UiState, WindowKind};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Result};

use crate::surface::Gfx;
use crate::system::{Waker, register_gdi_font, system_dark};
use crate::terminal::{PUMP_BUDGET, TerminalPane, set_terminal_waker};
use crate::viewer::ViewerHost;
use crate::window::{TIMER_PLACEHOLDER, TIMER_TICK, Window};

pub struct Shared {
    pub gfx: Gfx,
    pub state: RefCell<UiState>,
    pub backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    /// The viewer pipeline's hooks (stage 18g); `None` in the mock and offscreen runs.
    pub host: Option<Arc<dyn ViewerHost>>,
    windows: RefCell<Vec<Rc<Window>>>,
    tick_on: Cell<bool>,
    placeholder_on: Cell<bool>,
    /// The drawer's terminal: the model of the open shell and its renderer (made when the panel first shows it).
    pub terminal: RefCell<TerminalPane>,
}

thread_local! {
    static SHARED: RefCell<Option<Rc<Shared>>> = const { RefCell::new(None) };
}

/// The composition-tree handles of a window kind, for the renderer of stage 15 to attach its video visual to (C3
/// `NativeSurface`). Call on the UI thread, after the window exists.
pub fn native_surface(kind: WindowKind) -> Option<swapdesk_platform::NativeSurface> {
    SHARED.with(|s| s.borrow().as_ref().and_then(|sh| sh.window(kind)).and_then(|w| w.native_surface()))
}

/// The video rectangle of the viewer window in window pixels (stage 15 resizes its visual to it).
pub fn video_area() -> Option<(f32, f32, f32, f32)> {
    SHARED.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|sh| sh.window(WindowKind::Viewer))
            .and_then(|w| w.video_area())
            .map(|r| (r.x, r.y, r.w, r.h))
    })
}

/// Runs the shell until the main window closes. `state` is the starting view model (a mock scenario already applied,
/// or an empty one); `startup` are the commands to send at once after the window is up (R-UI-6).
pub fn run(state: UiState, backend: Arc<dyn UiBackend>, waker: Arc<Waker>, startup: Vec<Command>) -> Result<()> {
    run_with_host(state, backend, waker, startup, None)
}

/// As [`run`], with the viewer pipeline's hooks (see [`ViewerHost`]): the viewer window reports its video rectangle,
/// focus, mouse and keyboard to `host`.
pub fn run_with_host(
    state: UiState,
    backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    startup: Vec<Command>,
    host: Option<Arc<dyn ViewerHost>>,
) -> Result<()> {
    // SAFETY: called once at startup, before any window exists; failures (already set by a manifest) are harmless.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    register_gdi_font();
    // R-UI-10: UI-thread work over 16 ms goes to the debugger output with its name and duration, nothing else.
    swapdesk_ui::trace::set_hook(|e| {
        let line = format!("swapdesk: slow UI work {} took {} ms\0", e.name, e.millis);
        crate::system::trace_line(&line);
    });
    let gfx = Gfx::new()?;
    let mut state = state;
    // Files are received into this user's Downloads folder until the user picks another (the file panel).
    if state.receive_dir().is_none()
        && let Some(dir) = crate::files_dialogs::default_receive_dir()
    {
        let _ = state.set_receive_dir(dir);
    }
    let shared = Rc::new(Shared {
        gfx,
        state: RefCell::new(state),
        backend,
        waker: Arc::clone(&waker),
        host,
        windows: RefCell::new(Vec::new()),
        tick_on: Cell::new(false),
        placeholder_on: Cell::new(false),
        terminal: RefCell::new(TerminalPane::default()),
    });
    // Terminal output arrives on the pipe's reader thread and wakes this thread to pump and repaint.
    set_terminal_waker(Arc::clone(&waker) as Arc<dyn swapdesk_ui::UiWaker>);
    SHARED.with(|s| *s.borrow_mut() = Some(Rc::clone(&shared)));
    // The window appears before any answer; the requests go out right after, all together.
    let main = Window::create(&shared, WindowKind::Main)?;
    waker.set_window(main.hwnd());
    shared.windows.borrow_mut().push(main);
    shared.send_commands(startup);
    shared.refresh(Changed::ALL);
    shared.drain_backend();

    let mut msg = MSG::default();
    // SAFETY: the standard message loop.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    SHARED.with(|s| *s.borrow_mut() = None);
    Ok(())
}

impl Shared {
    pub fn window(&self, kind: WindowKind) -> Option<Rc<Window>> {
        self.windows.borrow().iter().find(|w| w.kind == kind).cloned()
    }

    fn main_hwnd(&self) -> Option<HWND> {
        self.window(WindowKind::Main).map(|w| w.hwnd())
    }

    /// Sends commands to the backend (never blocks) and remembers their tickets.
    pub fn send_commands(&self, commands: Vec<Command>) {
        for cmd in commands {
            let _g = SlowGuard::ui("UiBackend::send");
            let ticket = self.backend.send(cmd.clone());
            self.state.borrow_mut().track(ticket, &cmd);
        }
    }

    pub fn drain_backend(self: &Rc<Self>) {
        self.waker.drained();
        let events = self.backend.drain();
        let mut changed = Changed::NONE;
        self.state.borrow_mut().set_clock(crate::system::local_clock());
        for e in events {
            changed = changed.union(self.state.borrow_mut().apply(e));
        }
        let followups = self.state.borrow_mut().take_followups();
        self.send_commands(followups);
        self.pump_terminal();
        self.refresh(changed);
    }

    /// Feeds the open shell's model with the output that arrived (a byte budget a turn, so a burst never holds the
    /// thread, R-UI-1), writes back what the terminal answers the program, and repaints the viewer when the screen changed.
    pub fn pump_terminal(self: &Rc<Self>) {
        let want = {
            let st = self.state.borrow();
            matches!(st.term_phase(), swapdesk_ui::TermPhase::Open { .. } | swapdesk_ui::TermPhase::Exited { .. })
                .then(|| st.term_id())
                .flatten()
        };
        let pumped = {
            let mut pane = self.terminal.borrow_mut();
            pane.sync(want);
            pane.pump(PUMP_BUDGET)
        };
        if let (Some(term_id), false) = (pumped.id, pumped.replies.is_empty()) {
            self.send_commands(vec![Command::TermWrite { term_id, bytes: pumped.replies }]);
        }
        if pumped.repaint
            && let Some(w) = self.window(WindowKind::Viewer)
        {
            w.invalidate();
        }
        if pumped.more {
            // More is queued: another turn of the loop, not another loop here.
            swapdesk_ui::UiWaker::wake(&*self.waker);
        }
    }

    /// The user activated a control of `from`.
    pub fn activate(self: &Rc<Self>, id: ControlId, from: &Window) {
        let act = self.state.borrow_mut().activate(id);
        self.dispatch(act, from);
    }

    pub fn submit_edit(self: &Rc<Self>, id: EditId, text: String, from: &Window) {
        let act = self.state.borrow_mut().submit_edit(id, text);
        self.dispatch(act, from);
    }

    fn dispatch(self: &Rc<Self>, act: Activation, from: &Window) {
        match act.effect {
            Some(Effect::FocusEdit(e)) => from.focus_control(ControlId::Edit(e), true),
            Some(Effect::SubmitEdit(e)) => {
                let text = from.edit_value(e);
                let again = self.state.borrow_mut().submit_edit(e, text);
                if e == EditId::Chat {
                    from.clear_edit(e);
                }
                self.send_commands(again.commands);
                self.refresh(again.changed);
            }
            Some(Effect::OpenLogs) => {
                crate::system::open_logs_folder();
            }
            Some(effect @ (Effect::Menu(_) | Effect::SaveLog | Effect::PickFiles | Effect::PickReceiveFolder)) => {
                from.set_pending_effect(effect)
            }
            None => {}
        }
        self.send_commands(act.commands);
        self.refresh(act.changed);
    }

    /// Menus and dialogs: they wait for a person, so they run outside the slow-work guard.
    pub fn run_modal(self: &Rc<Self>, effect: Effect, from: &Window) {
        match effect {
            Effect::Menu(select) => {
                if let Some(index) = self.pick(select, from) {
                    let act = self.state.borrow_mut().choose(select, index);
                    self.send_commands(act.commands);
                    self.refresh(act.changed);
                }
            }
            Effect::SaveLog => {
                if let Some(path) =
                    crate::dialogs::save_log(from.hwnd(), &self.state.borrow().t(swapdesk_ui::MsgKey::ExportLog))
                {
                    self.send_commands(vec![Command::ActivityExport { path }]);
                }
            }
            Effect::PickFiles => {
                let paths = crate::files_dialogs::pick_files(
                    from.hwnd(),
                    &self.state.borrow().t(swapdesk_ui::MsgKey::ChooseFiles),
                );
                if !paths.is_empty() {
                    let act = self.state.borrow_mut().send_files(paths);
                    self.send_commands(act.commands);
                    self.refresh(act.changed);
                }
            }
            Effect::PickReceiveFolder => {
                let title = self.state.borrow().t(swapdesk_ui::MsgKey::FilesPickFolder);
                if let Some(dir) = crate::files_dialogs::pick_folder(from.hwnd(), &title) {
                    let act = self.state.borrow_mut().set_receive_dir(dir);
                    self.send_commands(act.commands);
                    self.refresh(act.changed);
                }
            }
            _ => {}
        }
    }

    /// The native popup menu of a select; the index of the chosen option.
    fn pick(&self, select: SelectId, from: &Window) -> Option<usize> {
        let options = self.state.borrow().select_options(select);
        if options.is_empty() {
            return None;
        }
        let (x, y) = from.screen_point_of(ControlId::Select(select));
        // SAFETY: a popup menu created, shown and destroyed here; the strings outlive the call.
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            for (i, (label, on)) in options.iter().enumerate() {
                let flags = if *on { MF_STRING | MF_CHECKED } else { MF_STRING };
                let _ = AppendMenuW(menu, flags, i + 1, &HSTRING::from(label.as_str()));
            }
            let chosen =
                TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, x, y, None, from.hwnd(), None);
            let _ = DestroyMenu(menu);
            (chosen.0 > 0).then(|| chosen.0 as usize - 1)
        }
    }

    pub fn tick(self: &Rc<Self>) {
        let c = self.state.borrow_mut().tick();
        self.refresh(c);
    }

    pub fn arm_placeholders(self: &Rc<Self>) {
        if let Some(h) = self.main_hwnd() {
            // SAFETY: our window; the placeholder timer is one-shot.
            unsafe {
                let _ = KillTimer(Some(h), TIMER_PLACEHOLDER);
            }
        }
        self.placeholder_on.set(false);
        let c = self.state.borrow_mut().arm_placeholders();
        self.refresh(c);
    }

    pub fn system_theme_changed(self: &Rc<Self>) {
        let dark = system_dark();
        let c = self.state.borrow_mut().set_dark(dark);
        let windows: Vec<Rc<Window>> = self.windows.borrow().clone();
        let kinds: Vec<WindowKind> = windows.iter().map(|w| w.kind).collect();
        for i in crate::window::theme_targets(&kinds) {
            windows[i].theme_changed();
        }
        self.refresh(c);
    }

    /// The viewer's close button: the session ends through the service, the window goes when it has.
    pub fn disconnect_viewer(self: &Rc<Self>) {
        let id = self.state.borrow().viewer_session().map(|s| s.id);
        if let Some(session_id) = id {
            self.send_commands(vec![Command::Disconnect { session_id }]);
        }
    }

    pub fn window_destroyed(&self, w: &Rc<Window>) {
        self.windows.borrow_mut().retain(|x| !Rc::ptr_eq(x, w));
    }

    /// Windows follow the state (a viewer exists while a session does), then whatever changed redraws.
    pub fn refresh(self: &Rc<Self>, changed: Changed) {
        let wanted = self.state.borrow().windows();
        for kind in &wanted {
            if self.window(*kind).is_none()
                && let Ok(w) = Window::create(self, *kind)
            {
                self.windows.borrow_mut().push(w);
            }
        }
        let extra: Vec<Rc<Window>> =
            self.windows.borrow().iter().filter(|w| !wanted.contains(&w.kind)).cloned().collect();
        for w in extra {
            // SAFETY: our window; WM_NCDESTROY removes it from the list.
            unsafe {
                let _ = DestroyWindow(w.hwnd());
            }
        }
        let windows: Vec<Rc<Window>> = self.windows.borrow().clone();
        for w in &windows {
            if changed.hits(w.kind) {
                w.invalidate();
            }
        }
        self.attention();
        self.sync_timers();
    }

    /// A request that needs an answer brings the main window up (restored, flashing) if it is not in front.
    fn attention(&self) {
        let st = self.state.borrow();
        let asks = st.incoming_session_open() || st.dialog(WindowKind::Main).is_some();
        drop(st);
        let Some(h) = self.main_hwnd() else { return };
        if !asks {
            return;
        }
        // SAFETY: our window.
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            if GetForegroundWindow() != h {
                let info = FLASHWINFO {
                    cbSize: size_of::<FLASHWINFO>() as u32,
                    hwnd: h,
                    dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                    uCount: 3,
                    dwTimeout: 0,
                };
                let _ = FlashWindowEx(&info);
            }
        }
    }

    /// Timers exist only while something needs them: no countdown, no pending command, no waiting data, no timer.
    fn sync_timers(&self) {
        let Some(h) = self.main_hwnd() else { return };
        let (tick, placeholder) = {
            let st = self.state.borrow();
            (st.needs_tick(), st.has_pending_fetch())
        };
        // SAFETY: our window.
        unsafe {
            if tick && !self.tick_on.get() {
                SetTimer(Some(h), TIMER_TICK, 1000, None);
                self.tick_on.set(true);
            } else if !tick && self.tick_on.get() {
                let _ = KillTimer(Some(h), TIMER_TICK);
                self.tick_on.set(false);
            }
            if placeholder && !self.placeholder_on.get() {
                SetTimer(Some(h), TIMER_PLACEHOLDER, 100, None);
                self.placeholder_on.set(true);
            } else if !placeholder && self.placeholder_on.get() {
                let _ = KillTimer(Some(h), TIMER_PLACEHOLDER);
                self.placeholder_on.set(false);
            }
        }
    }
}
