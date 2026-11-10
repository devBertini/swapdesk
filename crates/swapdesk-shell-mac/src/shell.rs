//! The shell's process-level state and run loop: the UI state, the backend, the windows that exist, the timers (set
//! only while something needs them, R-UI-1/8), and the glue between user actions and backend commands. Mirrors
//! `swapdesk-shell-win`'s `app.rs`.
//!
//! Apple documentation: `NSApplication` (`setActivationPolicy`, `run`, `setMainMenu`), `NSMenu`/`NSMenuItem`,
//! `NSTimer.scheduledTimer(timeInterval:target:selector:userInfo:repeats:)`, `NSNotificationCenter`
//! (`NSControlTextDidChangeNotification`), `NSDistributedNotificationCenter` with `AppleInterfaceThemeChangedNotification`
//! (the notification the system posts when the appearance changes), `NSSavePanel`, `NSWindowDelegate`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType, NSControl,
    NSEventType, NSImage, NSMenu, NSMenuItem, NSModalResponseOK, NSSavePanel, NSStatusBar, NSStatusItem, NSTextField,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSObject,
    NSObjectProtocol, NSString, NSTimer, NSUserDefaults, ns_string,
};
use swapdesk_ui::debounce::Debouncer;
use swapdesk_ui::tokens::layout;
use swapdesk_ui::trace::SlowGuard;
use swapdesk_ui::{
    Activation, Changed, Command, ControlId, EditId, Effect, MsgKey, Quality, SelectId, SpecialKeys, Theme, UiBackend,
    UiState, WindowKind,
};

use crate::banner_window::BannerWindow;
use crate::canvas::CanvasView;
use crate::input::ViewerInput;
use crate::model::*;
use crate::system::{Waker, local_clock, system_dark};
use crate::video::VideoSlot;
use crate::viewer_window::ViewerWindow;
use crate::views::*;

/// Receives a line the shell wants in the trace log.
pub type LaunchNote = Arc<dyn Fn(&str) + Send + Sync>;

/// What the other lanes plug into the viewer window.
#[derive(Default, Clone)]
pub struct ViewerHooks {
    /// The render lane: gets the video view's layer.
    pub video: Option<Arc<dyn VideoSlot>>,
    /// The input lane: gets the keyboard, mouse and focus events of the video view.
    pub input: Option<Arc<dyn ViewerInput>>,
    /// Draw the "Inverter" button in the toolbar (its slot is always laid out in the model).
    pub show_swap: bool,
    /// Told what the shell decided on its own at launch (login-item launch, status item), for the app's trace log.
    pub note: Option<LaunchNote>,
}

// ---------- the controller: the target of every control, the window and timer delegate ----------

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskController"]
    pub struct Controller;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for Controller {}

    // SAFETY: `NSWindowDelegate` has no safety requirements; the signatures are the protocol's.
    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, sender: &NSWindow) -> bool {
            with_shell(|s| s.should_close(sender)).unwrap_or(true)
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, n: &NSNotification) {
            with_shell(|s| s.resized(n));
        }

        #[unsafe(method(windowDidChangeBackingProperties:))]
        fn window_did_change_backing(&self, n: &NSNotification) {
            with_shell(|s| s.resized(n));
        }
    }

    // SAFETY: `NSApplicationDelegate` has no safety requirements; the signatures are the protocol's.
    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn application_did_finish_launching(&self, _n: &NSNotification) {
            with_shell(|s| s.did_finish_launching());
        }

        // A click on the Dock icon (or `open` on the running app) shows the main window again (D32).
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn application_should_handle_reopen(&self, _app: &NSApplication, _has_visible: bool) -> bool {
            with_shell(|s| s.show_main());
            false
        }

        // Closing the last window never ends the app: it keeps hosting (D32).
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn application_should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            false
        }
    }

    impl Controller {
        // SAFETY (all): the selectors are the ones the shell registers as control actions, timers and observers.
        #[unsafe(method(openMain:))]
        fn open_main(&self, _sender: &AnyObject) {
            with_shell(|s| s.show_main());
        }

        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: &AnyObject) {
            with_shell(|s| s.quit());
        }

        #[unsafe(method(act:))]
        fn act(&self, sender: &AnyObject) {
            with_shell(|s| s.on_action(sender));
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _t: &AnyObject) {
            with_shell(|s| s.tick());
        }

        #[unsafe(method(placeholders:))]
        fn placeholders(&self, _t: &AnyObject) {
            with_shell(|s| s.arm_placeholders());
        }

        #[unsafe(method(debounced:))]
        fn debounced(&self, _t: &AnyObject) {
            with_shell(|s| s.debounce_fired());
        }

        #[unsafe(method(textChanged:))]
        fn text_changed(&self, n: &NSNotification) {
            with_shell(|s| s.text_changed(n));
        }

        // Esc while the rename field is open: AppKit sends `cancelOperation:` up the responder chain (the window
        // forwards it to its delegate, this controller).
        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: Option<&AnyObject>) {
            with_shell(|s| s.cancel_rename());
        }

        #[unsafe(method(themeChanged:))]
        fn theme_changed(&self, _n: &NSNotification) {
            with_shell(|s| s.system_theme_changed());
        }
    }
);

impl Controller {
    fn create(mtm: MainThreadMarker) -> Retained<Controller> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `init` is NSObject's initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// `NSVariableStatusItemLength`: the item is as wide as its content.
const VARIABLE_STATUS_ITEM_LENGTH: f64 = -1.0;

// ---------- the shell ----------

struct MainWindow {
    window: Retained<NSWindow>,
    /// The content view: it paints the shared frame, takes the mouse and the keyboard and holds the text fields.
    canvas: Retained<CanvasView>,
}

/// The appearance `SWAPDESK_THEME=dark|light` asks for (development), `None` without it.
fn forced_appearance() -> Option<&'static str> {
    crate::locale::theme_override(std::env::var("SWAPDESK_THEME").ok().as_deref())
        .map(|dark| if dark { "NSAppearanceNameDarkAqua" } else { "NSAppearanceNameAqua" })
}

static SHOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static SHOT_TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Development: the file the main window is captured to once it is drawn (see `Shell::take_shot`).
pub fn set_shot(path: std::path::PathBuf) {
    let _ = SHOT.set(path);
}

pub struct Shell {
    mtm: MainThreadMarker,
    controller: Retained<Controller>,
    state: RefCell<UiState>,
    backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    hooks: ViewerHooks,
    main: MainWindow,
    viewer: RefCell<Option<ViewerWindow>>,
    /// The host banner, while another computer watches or controls this Mac.
    banner: RefCell<Option<BannerWindow>>,
    tick_timer: RefCell<Option<Retained<NSTimer>>>,
    placeholder_timer: RefCell<Option<Retained<NSTimer>>>,
    debounce_timer: RefCell<Option<Retained<NSTimer>>>,
    debounce: RefCell<Debouncer>,
    started: std::time::Instant,
    /// The theme whose `NSApp.appearance` is in force (the app starts with none: Sistema).
    appearance: std::cell::Cell<Theme>,
    /// The menu bar item: it keeps the app reachable while the main window is closed (D32).
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
}

thread_local! {
    static SHELL: RefCell<Option<Rc<Shell>>> = const { RefCell::new(None) };
}

fn with_shell<R>(f: impl FnOnce(&Rc<Shell>) -> R) -> Option<R> {
    let shell = SHELL.with(|s| s.borrow().clone())?;
    Some(f(&shell))
}

/// Reads the UI state for a paint. `None` when there is no shell, or while the shell itself is changing the state
/// (it marks the canvases dirty when it is done, so the paint comes again).
pub(crate) fn with_state<R>(f: impl FnOnce(&UiState) -> R) -> Option<R> {
    let shell = SHELL.with(|s| s.borrow().clone())?;
    let state = shell.state.try_borrow().ok()?;
    Some(f(&state))
}

/// A click on control `id` of the canvas of window `kind`.
pub(crate) fn activate_from(kind: WindowKind, id: ControlId) {
    with_shell(|s| s.activate_from(kind, id));
}

/// Return in a text field (or the keyboard leaving this computer's name): `text` is what input `id` holds.
pub(crate) fn submit_edit(id: EditId, text: String) {
    with_shell(|s| s.submit_edit(id, text));
}

/// Esc in the main window.
pub(crate) fn cancel_rename() {
    with_shell(|s| s.cancel_rename());
}

/// The main-queue block a canvas posts after a paint: the native text fields follow the frame's edit slots.
pub(crate) fn place_fields() {
    with_shell(|s| s.main.canvas.place_fields());
}

/// The main-queue block the waker posts.
pub(crate) fn wake_on_main() {
    with_shell(|s| s.drain_backend());
}

/// Runs the shell until the main window closes. `state` is the starting view model (a mock scenario already applied,
/// or an empty one); `startup` are the commands to send at once after the window is up (R-UI-6).
pub fn run(state: UiState, backend: Arc<dyn UiBackend>, waker: Arc<Waker>, startup: Vec<Command>) {
    run_with_host(state, backend, waker, startup, ViewerHooks::default());
}

/// As [`run`], with the other lanes' hooks (see [`ViewerHooks`]).
///
/// # Panics
/// When not called on the main thread (AppKit requires it).
pub fn run_with_host(
    state: UiState,
    backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    startup: Vec<Command>,
    hooks: ViewerHooks,
) {
    let mtm = MainThreadMarker::new().expect("the macOS shell runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // The main window waits for `applicationDidFinishLaunching:`, which knows whether the system launched the app as a
    // login item (then the window stays hidden, D32).
    let shell = start_with(mtm, state, backend, waker, startup, hooks, false);
    app.setDelegate(Some(ProtocolObject::from_ref(&*shell.controller)));
    shell.install_status_item();
    app.run();
    clear_shell();
}

/// Builds the shell, shows the main window and sends the startup commands, without entering the run loop.
pub(crate) fn start(
    mtm: MainThreadMarker,
    state: UiState,
    backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    startup: Vec<Command>,
    hooks: ViewerHooks,
) -> Rc<Shell> {
    start_with(mtm, state, backend, waker, startup, hooks, true)
}

/// As [`start`]; `show_now` false leaves the main window closed until [`Shell::show_main`].
fn start_with(
    mtm: MainThreadMarker,
    state: UiState,
    backend: Arc<dyn UiBackend>,
    waker: Arc<Waker>,
    startup: Vec<Command>,
    hooks: ViewerHooks,
    show_now: bool,
) -> Rc<Shell> {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // The system's font smoothing thickens the stems of text. The painter draws without it
    // (`CGContextSetShouldSmoothFonts`), and so must the native text fields over the frame, or a typed value looks
    // bolder than the text around it. CoreGraphics reads `AppleFontSmoothing` from the app's own preferences: a
    // registered default does not reach it (tried on macOS 27.2), so the value is written there, before any text is
    // drawn.
    NSUserDefaults::standardUserDefaults().setInteger_forKey(0, ns_string!("AppleFontSmoothing"));
    let controller = Controller::create(mtm);

    // R-UI-10: UI-thread work over 16 ms goes to stderr with its name and duration, nothing else.
    swapdesk_ui::trace::set_hook(|e| eprintln!("swapdesk: slow UI work {} took {} ms", e.name, e.millis));

    let mut state = state;
    // This toolbar has the quality select and the view-only switch: the state lists them.
    state.set_viewer_extras(true);
    install_menus(mtm, &app, &state);
    let (window, canvas) = make_main_window(mtm, &controller, &state.t(swapdesk_ui::MsgKey::AppName));
    if let Some(shot) = SHOT.get() {
        canvas.set_dump(shot.with_extension("layout.txt"));
    }
    let shell = Rc::new(Shell {
        mtm,
        controller: controller.clone(),
        state: RefCell::new(state),
        backend,
        waker,
        hooks,
        main: MainWindow { window, canvas },
        viewer: RefCell::new(None),
        banner: RefCell::new(None),
        tick_timer: RefCell::new(None),
        placeholder_timer: RefCell::new(None),
        debounce_timer: RefCell::new(None),
        debounce: RefCell::new(Debouncer::new()),
        started: std::time::Instant::now(),
        appearance: std::cell::Cell::new(Theme::System),
        status_item: RefCell::new(None),
    });
    SHELL.with(|s| *s.borrow_mut() = Some(Rc::clone(&shell)));
    shell.observe_system();
    shell.apply_forced_appearance();

    // The window appears before any answer; the requests go out right after, all together.
    if show_now {
        shell.show_main();
    }
    shell.send_commands(startup);
    shell.refresh(Changed::ALL);
    shell.drain_backend();
    shell
}

/// The main window is on screen (the self-test reads it).
pub fn main_window_visible() -> Option<bool> {
    with_shell(|s| s.main.window.isVisible())
}

/// What a click on the Dock icon does: the main window comes back (the self-test calls it).
pub fn reopen_main_window() {
    with_shell(|s| s.show_main());
}

/// The close button of the main window (the self-test calls it): the window goes, the app stays.
pub fn close_main_window() {
    with_shell(|s| s.main.window.performClose(None));
}

/// The menu bar item exists and is visible.
pub fn status_item_present() -> bool {
    with_shell(|s| s.status_item.borrow().as_ref().is_some_and(|i| i.isVisible())).unwrap_or(false)
}

/// Development (`--selftest`): a real click on a sidebar entry of the main window, sent through the window as AppKit
/// sends the mouse (`sendEvent:`). `Some(true)` when the screen changed, `None` when there was nothing to click.
pub fn selftest_click() -> Option<bool> {
    with_shell(|s| s.selftest_click()).flatten()
}

/// Development (`--mock <scenario> --selftest`): opens the first paired device, clicks "Renomear" and types `name`
/// and Return into the native field with real key events. `Some(true)` when the field held exactly `name` before the
/// Return; `None` when a step had nothing to act on. The backend answers the rename later: see
/// [`selftest_rename_end`].
pub fn selftest_rename_begin(name: &str) -> Option<bool> {
    with_shell(|s| s.selftest_rename_begin(name)).flatten()
}

/// Development: after [`selftest_rename_begin`] and the backend's answer, the device carries `name` and the rename
/// field is closed.
pub fn selftest_rename_end(name: &str) -> Option<bool> {
    with_shell(|s| {
        let state = s.state.borrow();
        state.selected_device().map(|d| d.name == name && state.renaming().is_none())
    })
    .flatten()
}

fn clear_shell() {
    SHELL.with(|s| *s.borrow_mut() = None);
}

fn make_main_window(
    mtm: MainThreadMarker,
    controller: &Controller,
    title: &str,
) -> (Retained<NSWindow>, Retained<CanvasView>) {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: the window is kept in `Shell`; `setReleasedWhenClosed(false)` is set below as AppKit requires.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame(0.0, 0.0, f64::from(layout::DESIGN_W), f64::from(layout::DESIGN_H)),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: see above.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns(title));
    window.setContentMinSize(size(f64::from(layout::MIN_W), f64::from(layout::MIN_H)));
    window.setDelegate(Some(ProtocolObject::from_ref(controller)));
    // The content view follows the window's size; the shell marks it dirty on a resize.
    let canvas = CanvasView::create(
        mtm,
        frame(0.0, 0.0, f64::from(layout::DESIGN_W), f64::from(layout::DESIGN_H)),
        WindowKind::Main,
    );
    window.setContentView(Some(&canvas));
    window.center();
    (window, canvas)
}

/// The application menu (quit) and the edit menu (so Cmd+V works in the address field).
fn install_menus(mtm: MainThreadMarker, app: &NSApplication, state: &UiState) {
    let bar = NSMenu::new(mtm);
    let item = |title: &str, action: objc2::runtime::Sel, key: &str| {
        // SAFETY: the actions are standard responder-chain selectors (`terminate:`, `cut:`, ...).
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), Some(action), &ns(key))
        }
    };
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&item(&state.t(MsgKey::MenuQuit), sel!(terminate:), "q"));
    app_item.setSubmenu(Some(&app_menu));
    bar.addItem(&app_item);

    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(&state.t(MsgKey::MenuEdit)));
    for (key, action, k) in [
        (MsgKey::MenuCut, sel!(cut:), "x"),
        (MsgKey::MenuCopy, sel!(copy:), "c"),
        (MsgKey::MenuPaste, sel!(paste:), "v"),
        (MsgKey::MenuSelectAll, sel!(selectAll:), "a"),
    ] {
        edit_menu.addItem(&item(&state.t(key), action, k));
    }
    edit_item.setSubmenu(Some(&edit_menu));
    bar.addItem(&edit_item);
    app.setMainMenu(Some(&bar));
}

impl Shell {
    fn target(&self) -> &AnyObject {
        self.controller.as_ref()
    }

    fn observe_system(&self) {
        let ctl: &AnyObject = self.target();
        // SAFETY: the observer is the controller, which lives as long as the shell; the selectors exist on it.
        unsafe {
            NSDistributedNotificationCenter::defaultCenter().addObserver_selector_name_object(
                ctl,
                sel!(themeChanged:),
                Some(ns_string!("AppleInterfaceThemeChangedNotification")),
                None,
            );
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                ctl,
                sel!(textChanged:),
                Some(ns_string!("NSControlTextDidChangeNotification")),
                None,
            );
        }
    }

    // ----- backend glue -----

    /// Sends commands to the backend (never blocks) and remembers their tickets.
    fn send_commands(&self, commands: Vec<Command>) {
        for cmd in commands {
            let _g = SlowGuard::ui("UiBackend::send");
            let ticket = self.backend.send(cmd.clone());
            self.state.borrow_mut().track(ticket, &cmd);
        }
    }

    fn drain_backend(&self) {
        self.waker.drained();
        let events = self.backend.drain();
        let mut changed = Changed::NONE;
        self.state.borrow_mut().set_clock(local_clock());
        for e in events {
            changed = changed.union(self.state.borrow_mut().apply(e));
        }
        let mut followups = self.state.borrow_mut().take_followups();
        let want = self.state.borrow_mut().take_display_request();
        followups.extend(want);
        self.send_commands(followups);
        self.refresh(changed);
    }

    fn dispatch(&self, act: Activation) {
        match act.effect {
            Some(Effect::FocusEdit(e)) => self.main.canvas.focus_control(ControlId::Edit(e), true),
            Some(Effect::SaveLog) => self.save_log(),
            // The Mac shell has no crash marker yet (D12 is Windows first): OpenLogs is never produced there.
            // The file panel's pickers (stage 44) are a Windows shell feature for now: the Mac panel is a later stage.
            Some(Effect::PickFiles) | Some(Effect::PickReceiveFolder) => {}
            // "Salvar" of the rename field: the field's text goes through `submit_edit` like Enter does.
            Some(Effect::SubmitEdit(e)) => {
                if let Some(text) = self.main.canvas.edit_text(e) {
                    let again = self.state.borrow_mut().submit_edit(e, text);
                    self.send_commands(again.commands);
                    self.refresh(again.changed);
                }
            }
            // A select's menu opens in `activate_from`, which knows the canvas the click came from.
            Some(Effect::Menu(_)) | Some(Effect::OpenLogs) | None => {}
        }
        self.send_commands(act.commands);
        self.refresh(act.changed);
    }

    fn activate(&self, id: ControlId) {
        let act = self.state.borrow_mut().activate(id);
        self.dispatch(act);
    }

    /// What a text field of a canvas holds goes to the state. An address waiting for its 150 ms goes now. A field can
    /// lose the keyboard in the middle of the shell's own work on the state: that call is dropped, the field keeps
    /// its text.
    fn submit_edit(&self, id: EditId, text: String) {
        if id == EditId::Address {
            self.debounce.borrow_mut().flush();
            if let Some(timer) = self.debounce_timer.borrow_mut().take() {
                timer.invalidate();
            }
        }
        let act = match self.state.try_borrow_mut() {
            Ok(mut state) => state.submit_edit(id, text),
            Err(_) => return,
        };
        self.dispatch(act);
    }

    /// A click on a painted control. A select answers with its menu, which waits for the person: it opens after the
    /// activation was dispatched, with no borrow of the state held.
    fn activate_from(&self, kind: WindowKind, id: ControlId) {
        let act = self.state.borrow_mut().activate(id);
        let select = match act.effect {
            Some(Effect::Menu(select)) => Some(select),
            _ => None,
        };
        self.dispatch(act);
        if let Some(select) = select {
            self.pick(kind, select);
        }
    }

    /// The canvas of window `kind`, where one paints it (the viewer and the banner come in M1.4).
    fn canvas(&self, kind: WindowKind) -> Option<Retained<CanvasView>> {
        match kind {
            WindowKind::Main => Some(self.main.canvas.clone()),
            WindowKind::Viewer | WindowKind::Banner => None,
        }
    }

    /// The menu of a select under its control; the choice goes back through `UiState::choose`.
    fn pick(&self, kind: WindowKind, select: SelectId) {
        let options = self.state.borrow().select_options(select);
        if options.is_empty() {
            return;
        }
        let Some(canvas) = self.canvas(kind) else { return };
        if let Some(index) = canvas.pick(&options, ControlId::Select(select)) {
            let act = self.state.borrow_mut().choose(select, index);
            self.dispatch(act);
        }
    }

    /// "Where to save the log" waits for a person, so it runs outside the slow-work guard.
    fn save_log(&self) {
        let title = self.state.borrow().t(swapdesk_ui::MsgKey::ExportLog);
        let panel = NSSavePanel::savePanel(self.mtm);
        panel.setTitle(Some(&ns(&title)));
        panel.setNameFieldStringValue(&ns("swapdesk-atividade.csv"));
        if panel.runModal() == NSModalResponseOK
            && let Some(path) = panel.URL().and_then(|u| u.path())
        {
            self.send_commands(vec![Command::ActivityExport { path: std::path::PathBuf::from(path.to_string()) }]);
        }
    }

    // ----- what AppKit tells the shell -----

    fn on_action(&self, sender: &AnyObject) {
        let Some(control) = sender.downcast_ref::<NSControl>() else { return };
        let tag = control.tag();
        let (idx, parity) = ((tag / WINDOWS) as usize, tag % WINDOWS);
        let action = match parity {
            // The main window is painted: its controls are not AppKit controls.
            PARITY_MAIN => None,
            PARITY_VIEWER => self.viewer.borrow().as_ref().and_then(|v| v.actions.borrow().get(idx).copied()),
            _ => self.banner.borrow().as_ref().and_then(|b| b.actions.borrow().get(idx).copied()),
        };
        let Some(action) = action else { return };
        let _g = SlowGuard::ui("on_action");
        let picked =
            sender.downcast_ref::<objc2_app_kit::NSPopUpButton>().map(|p| p.indexOfSelectedItem().max(0) as usize);
        match action {
            Action::Control(id) => self.activate(id),
            // The rename field acts on Return only: ending the editing by a click (on "Cancelar") must not save.
            Action::Edit(EditId::Rename) if !self.return_pressed() => {}
            Action::Edit(e) => {
                let text =
                    sender.downcast_ref::<NSTextField>().map(|f| f.stringValue().to_string()).unwrap_or_default();
                self.debounce.borrow_mut().flush();
                let act = self.state.borrow_mut().submit_edit(e, text);
                self.dispatch(act);
            }
            Action::Select(select) => {
                let act = self.state.borrow_mut().choose(select, picked.unwrap_or(0));
                self.dispatch(act);
            }
            Action::ToolScreen => {
                let id = picked.and_then(|i| self.state.borrow().viewer_displays().get(i).map(|d| d.id));
                if let Some(display_id) = id {
                    let act = self.state.borrow_mut().choose_display(display_id);
                    self.dispatch(act);
                }
            }
            Action::ToolQuality => {
                let q = Quality::ALL[picked.unwrap_or(0).min(2)];
                let act = self.state.borrow_mut().choose_quality(q);
                self.dispatch(act);
            }
            Action::ToolKeys => {
                let key = picked.and_then(|i| i.checked_sub(1)).and_then(|i| SpecialKeys::ALL.get(i).copied());
                if let Some(keys) = key {
                    let act = self.state.borrow_mut().send_keys(keys);
                    self.dispatch(act);
                }
            }
            Action::ToolSwitch(tool) => match tool {
                SwitchTool::Fit => self.activate(ControlId::TbFit),
                SwitchTool::Audio => self.activate(ControlId::TbAudio),
                SwitchTool::Grab => self.activate(ControlId::TbGrab),
                SwitchTool::ViewOnly => self.activate(ControlId::TbViewOnly),
            },
            Action::ToolSwap => self.activate(ControlId::TbSwap),
            Action::StripCancel(session_id) => self.send_commands(vec![Command::Disconnect { session_id }]),
            Action::StripDismiss => {
                let changed = self.state.borrow_mut().dismiss_connect();
                self.refresh(changed);
            }
        }
    }

    /// The event being handled is the Return or Enter key.
    fn return_pressed(&self) -> bool {
        NSApplication::sharedApplication(self.mtm)
            .currentEvent()
            .is_some_and(|e| e.r#type() == NSEventType::KeyDown && matches!(e.keyCode(), 36 | 76))
    }

    /// Esc: closes the rename field without saving.
    fn cancel_rename(&self) {
        if self.state.borrow().renaming().is_some() {
            self.activate(ControlId::RenameCancel);
        }
    }

    /// A key in a text field: the address field acts 150 ms after the last key (R-UI-4).
    fn text_changed(&self, n: &NSNotification) {
        let Some(obj) = n.object() else { return };
        let Some(field) = obj.downcast_ref::<NSTextField>() else { return };
        if self.main.canvas.edit_of(field) != Some(EditId::Address) {
            return;
        }
        let now = self.started.elapsed().as_millis() as u64;
        self.debounce.borrow_mut().input(&field.stringValue().to_string(), now);
        self.set_timer(&self.debounce_timer, 0.15, false, sel!(debounced:));
    }

    fn debounce_fired(&self) {
        *self.debounce_timer.borrow_mut() = None;
        let now = self.started.elapsed().as_millis() as u64;
        if let Some(text) = self.debounce.borrow_mut().poll(now) {
            let act = self.state.borrow_mut().submit_edit(EditId::Address, text);
            self.dispatch(act);
        }
    }

    fn should_close(&self, sender: &NSWindow) -> bool {
        let is_viewer = self.viewer.borrow().as_ref().is_some_and(|v| std::ptr::eq(&*v.window, sender));
        if is_viewer {
            // The close button ends the session through the service; the window goes when the session has.
            let id = self.state.borrow().viewer_session().map(|s| s.id);
            if let Some(session_id) = id {
                self.send_commands(vec![Command::Disconnect { session_id }]);
            }
            return false;
        }
        true
    }

    /// The main window comes up and takes the focus: the Dock icon, the status item, a request that needs an answer.
    fn show_main(&self) {
        if self.main.window.isMiniaturized() {
            self.main.window.deminiaturize(None);
        }
        self.main.window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
    }

    /// The only ways out are the app menu's Quit (Cmd+Q) and the status item's Quit; closing the window just hides it.
    fn quit(&self) {
        NSApplication::sharedApplication(self.mtm).terminate(None);
    }

    fn note(&self, text: &str) {
        if let Some(note) = &self.hooks.note {
            note(text);
        }
    }

    /// `applicationDidFinishLaunching:`: the launch Apple event is still the one being handled, so it can say whether
    /// the system started the app as a login item. Then the window stays hidden and only the status item shows.
    fn did_finish_launching(&self) {
        let login = crate::launch::launched_as_login_item();
        self.note(&format!("launched as login item: {login}"));
        if !login {
            self.show_main();
        }
    }

    /// The menu bar item: a template symbol (a short text when the symbol is missing) and its two-entry menu.
    fn install_status_item(&self) {
        let (open, quit) = {
            let st = self.state.borrow();
            (st.t(MsgKey::StatusOpen), st.t(MsgKey::StatusQuit))
        };
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(VARIABLE_STATUS_ITEM_LENGTH);
        if let Some(button) = item.button(self.mtm) {
            let symbol =
                NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("display"), Some(&ns(&open)));
            match symbol {
                Some(image) => {
                    image.setTemplate(true);
                    button.setImage(Some(&image));
                }
                None => button.setTitle(ns_string!("SD")),
            }
        }
        let menu = NSMenu::new(self.mtm);
        for (title, action) in [(open, sel!(openMain:)), (quit, sel!(quitApp:))] {
            // SAFETY: the actions are the controller's `openMain:` and `quitApp:`.
            let entry = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm),
                    &ns(&title),
                    Some(action),
                    &ns(""),
                )
            };
            // SAFETY: the target is the controller, which lives as long as the shell; `setTarget` keeps it unretained.
            unsafe { entry.setTarget(Some(self.target())) };
            menu.addItem(&entry);
        }
        item.setMenu(Some(&menu));
        *self.status_item.borrow_mut() = Some(item);
        self.note("status item installed");
    }

    fn resized(&self, n: &NSNotification) {
        let Some(obj) = n.object() else { return };
        if std::ptr::eq(&*self.main.window as *const NSWindow as *const AnyObject, &*obj as *const AnyObject) {
            self.main.canvas.setNeedsDisplay(true);
        }
        let is_viewer = self.viewer.borrow().as_ref().is_some_and(|v| {
            std::ptr::eq(&*v.window as *const NSWindow as *const AnyObject, &*obj as *const AnyObject)
        });
        if is_viewer && let (Some(slot), Some(v)) = (&self.hooks.video, self.viewer.borrow().as_ref()) {
            slot.resized(v.video.area());
        }
    }

    fn tick(&self) {
        let c = self.state.borrow_mut().tick();
        self.refresh(c);
    }

    fn arm_placeholders(&self) {
        *self.placeholder_timer.borrow_mut() = None;
        let c = self.state.borrow_mut().arm_placeholders();
        self.refresh(c);
    }

    fn system_theme_changed(&self) {
        let dark = system_dark();
        let c = self.state.borrow_mut().set_dark(dark);
        self.refresh(c);
    }

    /// Gives `NSApp` the appearance the theme setting asks for (Aqua, DarkAqua, or none for Sistema), once per change:
    /// the native controls and the title bars follow it. Back on Sistema the OS's own appearance is read again, since
    /// while one was forced `effectiveAppearance` showed the forced one.
    /// Sets the appearance once at launch: under `SWAPDESK_THEME` (development) the app's own controls must follow
    /// the forced theme too, or a light card gets the dark system's white button titles.
    fn apply_forced_appearance(&self) {
        if let Some(name) = forced_appearance() {
            let app = NSApplication::sharedApplication(self.mtm);
            app.setAppearance(NSAppearance::appearanceNamed(&NSString::from_str(name)).as_deref());
        }
    }

    fn sync_appearance(&self) -> Changed {
        let theme = self.state.borrow().theme();
        if self.appearance.replace(theme) == theme {
            return Changed::NONE;
        }
        let app = NSApplication::sharedApplication(self.mtm);
        let appearance = appearance_name(theme)
            .or_else(|| (theme == Theme::System).then(forced_appearance).flatten())
            .and_then(|n| NSAppearance::appearanceNamed(&NSString::from_str(n)));
        app.setAppearance(appearance.as_deref());
        if theme == Theme::System {
            let dark = system_dark();
            return self.state.borrow_mut().set_dark(dark).union(Changed::MAIN);
        }
        Changed::MAIN
    }

    // ----- drawing -----

    /// Windows follow the state (a viewer exists while a session does); then what changed is painted again (the main
    /// window) or rebuilt when its model differs from the last one drawn (the viewer and the banner).
    fn refresh(&self, changed: Changed) {
        let changed = changed.union(self.sync_appearance());
        if changed.is_none() {
            self.sync_timers();
            return;
        }
        self.sync_viewer();
        if changed.hits(WindowKind::Banner) || changed.hits(WindowKind::Main) {
            self.sync_banner();
        }
        if changed.hits(WindowKind::Main) || changed.hits(WindowKind::Banner) {
            self.main.canvas.setNeedsDisplay(true);
            self.take_shot();
        }
        if changed.hits(WindowKind::Viewer)
            && let Some(v) = self.viewer.borrow().as_ref()
            && let Some(model) = viewer_model(&self.state.borrow())
        {
            v.update(self.mtm, self.target(), &model, self.hooks.show_swap);
        }
        self.attention();
        self.sync_timers();
    }

    /// The banner follows the host session: created with it, rebuilt when its line changes, gone with it.
    fn sync_banner(&self) {
        let model = banner_model(&self.state.borrow());
        let mut slot = self.banner.borrow_mut();
        match model {
            Some(m) => {
                let first = slot.is_none();
                let b = slot.get_or_insert_with(|| BannerWindow::create(self.mtm));
                b.update(self.mtm, self.target(), &m);
                if first && let Some(dir) = crate::banner_window::snapshot_dir() {
                    b.snapshot(dir);
                }
            }
            None => {
                if let Some(b) = slot.take() {
                    b.close();
                }
            }
        }
    }

    fn sync_viewer(&self) {
        let model = viewer_model(&self.state.borrow());
        let exists = self.viewer.borrow().is_some();
        match (model, exists) {
            (Some(m), false) => {
                let delegate = ProtocolObject::from_ref(&*self.controller);
                let v = ViewerWindow::create(self.mtm, delegate, &m.title);
                v.update(self.mtm, self.target(), &m, self.hooks.show_swap);
                if let (Some(slot), Some(surface)) = (&self.hooks.video, v.surface()) {
                    slot.attached(&surface);
                }
                if let Some(input) = &self.hooks.input {
                    v.video.set_input(Some(Arc::clone(input)));
                }
                *self.viewer.borrow_mut() = Some(v);
            }
            (None, true) => {
                if let Some(v) = self.viewer.borrow_mut().take() {
                    if let Some(slot) = &self.hooks.video {
                        slot.detached();
                    }
                    v.video.set_input(None);
                    v.window.setDelegate(None);
                    v.window.close();
                }
            }
            _ => {}
        }
    }

    /// Development (`--mock <scenario> --shot <file>`): once the main window is drawn, `screencapture` takes it alone
    /// a second later (the app's own screen recording grant; nothing is asked), then the app quits.
    fn take_shot(&self) {
        let Some(path) = SHOT.get() else { return };
        if SHOT_TAKEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // The main window's content, painted into a bitmap: it needs no screen, so it comes out with the Mac locked too.
        self.main.canvas.save_png(&path.with_extension("content.png"));
        // Where each native text field sits against the slot the frame drew for it.
        let _ = std::fs::write(path.with_extension("fields.txt"), self.main.canvas.fields_dump());
        // A scenario with a session films the viewer window, any other the main window.
        let window = self.viewer.borrow().as_ref().map_or(self.main.window.windowNumber(), |v| v.window.windowNumber());
        let path = path.clone();
        // The view tree beside the picture (class, frame, hidden, layer), for a capture that looks wrong.
        if let Some(v) = self.viewer.borrow().as_ref()
            && let Some(root) = v.window.contentView()
        {
            fn walk(v: &NSView, depth: usize, out: &mut String) {
                let f = v.frame();
                out.push_str(&format!(
                    "{:indent$}{} {:.0},{:.0} {:.0}x{:.0}{}{}
",
                    "",
                    v.class().name().to_string_lossy(),
                    f.origin.x,
                    f.origin.y,
                    f.size.width,
                    f.size.height,
                    if v.isHidden() { " hidden" } else { "" },
                    if v.wantsLayer() { " layer" } else { "" },
                    indent = depth * 2
                ));
                let subs = v.subviews();
                for i in 0..subs.count() {
                    walk(&subs.objectAtIndex(i), depth + 1, out);
                }
            }
            let mut out = String::new();
            walk(&root, 0, &mut out);
            let _ = std::fs::write(path.with_extension("views.txt"), out);
        }
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let status = std::process::Command::new("/usr/sbin/screencapture")
                .args(["-x", "-o", &format!("-l{window}"), &path.to_string_lossy()])
                .status();
            std::process::exit(if status.is_ok_and(|s| s.success()) { 0 } else { 1 });
        });
    }

    fn selftest_click(&self) -> Option<bool> {
        let canvas = &self.main.canvas;
        if !canvas.painted() && !canvas.paint_now() {
            return None;
        }
        let before = self.state.borrow().screen();
        let target = if before == swapdesk_ui::Screen::Settings { ControlId::NavHome } else { ControlId::NavSettings };
        if !canvas.send_click(target) {
            return None;
        }
        Some(self.state.borrow().screen() != before)
    }

    /// A click on control `id` of the main window as it is now: the frame is painted first, so the control is where
    /// the state puts it.
    fn selftest_press(&self, id: ControlId) -> Option<()> {
        let canvas = &self.main.canvas;
        (canvas.paint_now() && canvas.send_click(id)).then_some(())
    }

    fn selftest_rename_begin(&self, name: &str) -> Option<bool> {
        let device = self.state.borrow().devices().iter().position(|d| d.paired)?;
        self.selftest_press(ControlId::Device(u16::try_from(device).ok()?))?;
        self.selftest_press(ControlId::RenameDevice)?;
        // The rename field appears with the next paint, prefilled and selected: typing replaces the old name.
        let canvas = &self.main.canvas;
        canvas.paint_now();
        canvas.edit_text(EditId::Rename)?;
        canvas.send_keys(name);
        let typed = canvas.edit_text(EditId::Rename)? == name;
        canvas.send_keys("\r");
        Some(typed)
    }

    /// A request that needs an answer brings the main window up.
    fn attention(&self) {
        let asks = {
            let st = self.state.borrow();
            st.incoming_session_open() || st.dialog(WindowKind::Main).is_some()
        };
        if asks {
            self.show_main();
        }
    }

    fn set_timer(
        &self,
        slot: &RefCell<Option<Retained<NSTimer>>>,
        secs: f64,
        repeats: bool,
        selector: objc2::runtime::Sel,
    ) {
        if let Some(old) = slot.borrow_mut().take() {
            old.invalidate();
        }
        // SAFETY: the target is the controller, which implements `selector`; the timer is invalidated before it is dropped.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                secs,
                self.target(),
                selector,
                None,
                repeats,
            )
        };
        *slot.borrow_mut() = Some(timer);
    }

    /// Timers exist only while something needs them: no countdown, no pending command, no waiting data, no timer.
    fn sync_timers(&self) {
        let (tick, placeholder) = {
            let st = self.state.borrow();
            (st.needs_tick(), st.has_pending_fetch())
        };
        let ticking = self.tick_timer.borrow().is_some();
        if tick && !ticking {
            self.set_timer(&self.tick_timer, 1.0, true, sel!(tick:));
        } else if !tick
            && ticking
            && let Some(t) = self.tick_timer.borrow_mut().take()
        {
            t.invalidate();
        }
        let waiting = self.placeholder_timer.borrow().is_some();
        if placeholder && !waiting {
            self.set_timer(&self.placeholder_timer, 0.1, false, sel!(placeholders:));
        } else if !placeholder
            && waiting
            && let Some(t) = self.placeholder_timer.borrow_mut().take()
        {
            t.invalidate();
        }
    }
}

// Selecting a pop-up option by `SelectId` is handled in `on_action`; keep the import used for the match arm.
const _: Option<SelectId> = None;

#[doc(hidden)]
pub mod selftest;
