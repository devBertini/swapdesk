//! The shell's process-level state and run loop: the UI state, the backend, the windows that exist, the timers (set
//! only while something needs them, R-UI-1/8), and the glue between user actions and backend commands. Mirrors
//! `swapdesk-shell-win`'s `app.rs`.
//!
//! Apple documentation: `NSApplication` (`setActivationPolicy`, `run`, `setMainMenu`), `NSMenu`/`NSMenuItem`,
//! `NSTimer.scheduledTimer(timeInterval:target:selector:userInfo:repeats:)`, `NSNotificationCenter`
//! (`NSControlTextDidChangeNotification`), `NSDistributedNotificationCenter` with `AppleInterfaceThemeChangedNotification`
//! (the notification the system posts when the appearance changes), `NSSavePanel`, `NSWindowDelegate`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType, NSBox, NSBoxType,
    NSControl, NSMenu, NSMenuItem, NSModalResponseOK, NSSavePanel, NSScrollView, NSTextField, NSTitlePosition, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSObject,
    NSObjectProtocol, NSPoint, NSRange, NSTimer, ns_string,
};
use swapdesk_ui::debounce::Debouncer;
use swapdesk_ui::tokens::{layout, palette};
use swapdesk_ui::trace::SlowGuard;
use swapdesk_ui::{
    Activation, Changed, Command, ControlId, EditId, Effect, MsgKey, Quality, SelectId, SpecialKeys, UiBackend,
    UiState, WindowKind,
};

use crate::input::ViewerInput;
use crate::model::*;
use crate::system::{Waker, local_clock, system_dark};
use crate::video::VideoSlot;
use crate::viewer_window::ViewerWindow;
use crate::views::*;

/// What the other lanes plug into the viewer window.
#[derive(Default, Clone)]
pub struct ViewerHooks {
    /// The render lane: gets the video view's layer.
    pub video: Option<Arc<dyn VideoSlot>>,
    /// The input lane: gets the keyboard, mouse and focus events of the video view.
    pub input: Option<Arc<dyn ViewerInput>>,
    /// Draw the "Inverter" button in the toolbar (its slot is always laid out in the model).
    pub show_swap: bool,
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

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            with_shell(|s| s.main_closed());
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

    impl Controller {
        // SAFETY (all): the selectors are the ones the shell registers as control actions, timers and observers.
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

// ---------- the shell ----------

struct MainWindow {
    window: Retained<NSWindow>,
    actions: RefCell<Vec<Action>>,
    fields: RefCell<Vec<(EditId, Retained<NSTextField>)>>,
    last: RefCell<Option<MainModel>>,
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
    tick_timer: RefCell<Option<Retained<NSTimer>>>,
    placeholder_timer: RefCell<Option<Retained<NSTimer>>>,
    debounce_timer: RefCell<Option<Retained<NSTimer>>>,
    debounce: RefCell<Debouncer>,
    started: std::time::Instant,
    quitting: Cell<bool>,
}

thread_local! {
    static SHELL: RefCell<Option<Rc<Shell>>> = const { RefCell::new(None) };
}

fn with_shell<R>(f: impl FnOnce(&Rc<Shell>) -> R) -> Option<R> {
    let shell = SHELL.with(|s| s.borrow().clone())?;
    Some(f(&shell))
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
    let _shell = start(mtm, state, backend, waker, startup, hooks);
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
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let controller = Controller::create(mtm);

    // R-UI-10: UI-thread work over 16 ms goes to stderr with its name and duration, nothing else.
    swapdesk_ui::trace::set_hook(|e| eprintln!("swapdesk: slow UI work {} took {} ms", e.name, e.millis));

    let mut state = state;
    // This toolbar has the quality select and the view-only switch: the state lists them.
    state.set_viewer_extras(true);
    install_menus(mtm, &app, &state);
    let window = make_main_window(mtm, &controller, &state.t(swapdesk_ui::MsgKey::AppName));
    let shell = Rc::new(Shell {
        mtm,
        controller: controller.clone(),
        state: RefCell::new(state),
        backend,
        waker,
        hooks,
        main: MainWindow {
            window,
            actions: RefCell::new(Vec::new()),
            fields: RefCell::new(Vec::new()),
            last: RefCell::new(None),
        },
        viewer: RefCell::new(None),
        tick_timer: RefCell::new(None),
        placeholder_timer: RefCell::new(None),
        debounce_timer: RefCell::new(None),
        debounce: RefCell::new(Debouncer::new()),
        started: std::time::Instant::now(),
        quitting: Cell::new(false),
    });
    SHELL.with(|s| *s.borrow_mut() = Some(Rc::clone(&shell)));
    shell.observe_system();

    // The window appears before any answer; the requests go out right after, all together.
    shell.main.window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    shell.send_commands(startup);
    shell.refresh(Changed::ALL);
    shell.drain_backend();
    shell
}

fn clear_shell() {
    SHELL.with(|s| *s.borrow_mut() = None);
}

fn make_main_window(mtm: MainThreadMarker, controller: &Controller, title: &str) -> Retained<NSWindow> {
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
    window.center();
    window
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
            Some(Effect::FocusEdit(e)) => {
                let fields = self.main.fields.borrow();
                if let Some((_, f)) = fields.iter().find(|(id, _)| *id == e) {
                    self.main.window.makeFirstResponder(Some(f));
                }
            }
            Some(Effect::SaveLog) => self.save_log(),
            // The Mac shell has no crash marker yet (D12 is Windows first): OpenLogs is never produced there.
            // The file panel's pickers (stage 44) are a Windows shell feature for now: the Mac panel is a later stage.
            Some(Effect::PickFiles) | Some(Effect::PickReceiveFolder) => {}
            Some(Effect::SubmitEdit(_)) | Some(Effect::Menu(_)) | Some(Effect::OpenLogs) | None => {}
        }
        self.send_commands(act.commands);
        self.refresh(act.changed);
    }

    fn activate(&self, id: ControlId) {
        let act = self.state.borrow_mut().activate(id);
        self.dispatch(act);
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
        let (idx, parity) = ((tag / 2) as usize, tag % 2);
        let action = if parity == 0 {
            self.main.actions.borrow().get(idx).copied()
        } else {
            self.viewer.borrow().as_ref().and_then(|v| v.actions.borrow().get(idx).copied())
        };
        let Some(action) = action else { return };
        let _g = SlowGuard::ui("on_action");
        let picked =
            sender.downcast_ref::<objc2_app_kit::NSPopUpButton>().map(|p| p.indexOfSelectedItem().max(0) as usize);
        match action {
            Action::Control(id) => self.activate(id),
            Action::Edit(e) => {
                let text =
                    sender.downcast_ref::<NSTextField>().map(|f| f.stringValue().to_string()).unwrap_or_default();
                self.debounce.borrow_mut().flush();
                let act = self.state.borrow_mut().submit_edit(e, text);
                if e == EditId::Address {
                    self.clear_field(e);
                }
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

    fn clear_field(&self, e: EditId) {
        if let Some((_, f)) = self.main.fields.borrow().iter().find(|(id, _)| *id == e) {
            f.setStringValue(&ns(""));
        }
    }

    /// A key in a text field: the address field acts 150 ms after the last key (R-UI-4).
    fn text_changed(&self, n: &NSNotification) {
        let Some(obj) = n.object() else { return };
        let Some(field) = obj.downcast_ref::<NSTextField>() else { return };
        let Some(Action::Edit(EditId::Address)) =
            self.main.actions.borrow().get((field.tag() / 2) as usize).copied().filter(|_| field.tag() % 2 == 0)
        else {
            return;
        };
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

    fn main_closed(&self) {
        // Only the main window's close ends the program; the viewer's own `close()` after a session also lands here.
        if self.main.window.isVisible() || self.quitting.get() {
            return;
        }
        self.quitting.set(true);
        NSApplication::sharedApplication(self.mtm).terminate(None);
    }

    fn resized(&self, n: &NSNotification) {
        let Some(obj) = n.object() else { return };
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

    // ----- drawing -----

    /// Windows follow the state (a viewer exists while a session does); then what changed is rebuilt, and only when
    /// its model differs from the last one drawn.
    fn refresh(&self, changed: Changed) {
        if changed.is_none() {
            self.sync_timers();
            return;
        }
        self.sync_viewer();
        if changed.hits(WindowKind::Main) || changed.hits(WindowKind::Banner) {
            self.rebuild_main();
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

    fn rebuild_main(&self) {
        let model = main_model(&self.state.borrow());
        if self.main.last.borrow().as_ref() == Some(&model) {
            return;
        }
        let mtm = self.mtm;
        let pal = palette(model.dark);
        let win = &self.main.window;
        let bounds = win
            .contentView()
            .map_or(frame(0.0, 0.0, f64::from(layout::DESIGN_W), f64::from(layout::DESIGN_H)), |v| v.bounds());
        let (w, h) = (bounds.size.width, bounds.size.height);

        // Keep what the person is typing across the rebuild.
        let values: Vec<(EditId, String)> =
            self.main.fields.borrow().iter().map(|(e, f)| (*e, f.stringValue().to_string())).collect();
        let focused = self
            .main
            .fields
            .borrow()
            .iter()
            .find(|(_, f)| {
                f.currentEditor().is_some_and(|ed| {
                    win.firstResponder().is_some_and(|r| {
                        std::ptr::eq(&*r as *const _ as *const AnyObject, &*ed as *const _ as *const AnyObject)
                    })
                })
            })
            .map(|(e, _)| *e);

        let mut actions = self.main.actions.borrow_mut();
        actions.clear();
        let mut fields = Vec::new();
        let mut ctx = Ctx { mtm, target: self.target(), pal, actions: &mut actions, parity: 0, fields: &mut fields };
        let root = PaintedView::create(mtm, bounds, Some(pal.canvas));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        match &model.layout {
            MainLayout::Browse { sidebar: sb, content } => {
                let side = sidebar(&mut ctx, sb, h, &values);
                root.addSubview(&side);
                let cw = w - f64::from(layout::SIDEBAR_W);
                let mut views: Vec<Retained<NSView>> = Vec::new();
                if let Some(s) = &model.strip {
                    views.push(strip(&mut ctx, s, 560.0));
                }
                let body = blocks_stack(&mut ctx, content, 560.0, &values);
                views.push(view_of(&body));
                let col = vstack(&ctx, &views, 16.0);
                let inner = fit(&col);
                let doc = PaintedView::create(mtm, frame(0.0, 0.0, cw, inner.height + 64.0), None);
                doc.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
                col.setFrameOrigin(NSPoint::new(32.0, 32.0));
                doc.addSubview(&col);
                let scroll = NSScrollView::initWithFrame(
                    NSScrollView::alloc(mtm),
                    frame(f64::from(layout::SIDEBAR_W), 0.0, cw, h),
                );
                scroll.setHasVerticalScroller(true);
                scroll.setDrawsBackground(false);
                scroll.setDocumentView(Some(&doc));
                scroll.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
                );
                root.addSubview(&scroll);
            }
            MainLayout::Card { blocks } => {
                let mut views: Vec<Retained<NSView>> = Vec::new();
                if let Some(s) = &model.strip {
                    views.push(strip(&mut ctx, s, layout::DIALOG_W as f64 - 64.0));
                }
                let stack = blocks_stack(&mut ctx, blocks, f64::from(layout::DIALOG_W) - 64.0, &values);
                views.push(view_of(&stack));
                let col = vstack(&ctx, &views, 16.0);
                let inner = fit(&col);
                let card = NSBox::new(mtm);
                card.setBoxType(NSBoxType::Custom);
                card.setTitlePosition(NSTitlePosition::NoTitle);
                card.setBorderWidth(1.0);
                card.setBorderColor(&color(pal.line));
                card.setCornerRadius(f64::from(layout::DIALOG_RADIUS));
                card.setFillColor(&color(pal.surface));
                card.setContentViewMargins(size(32.0, 28.0));
                card.setContentView(Some(&view_of(&col)));
                let cw = f64::from(layout::DIALOG_W).max(inner.width + 64.0);
                let ch = inner.height + 56.0;
                card.setFrame(frame((w - cw) / 2.0, ((h - ch) / 2.0).max(24.0), cw, ch));
                card.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin,
                );
                root.addSubview(&card);
            }
        }
        win.setTitle(&ns(&model.title));
        win.setContentView(Some(&view_of(&root)));
        *self.main.fields.borrow_mut() = fields;

        if let Some(e) = focused
            && let Some((_, f)) = self.main.fields.borrow().iter().find(|(id, _)| *id == e)
        {
            win.makeFirstResponder(Some(f));
            if let Some(ed) = f.currentEditor() {
                let len = f.stringValue().length();
                ed.setSelectedRange(NSRange::new(len, 0));
            }
        }
        *self.main.last.borrow_mut() = Some(model);
    }

    /// A request that needs an answer brings the main window up.
    fn attention(&self) {
        let asks = {
            let st = self.state.borrow();
            st.incoming_session_open() || st.dialog(WindowKind::Main).is_some()
        };
        if asks {
            if self.main.window.isMiniaturized() {
                self.main.window.deminiaturize(None);
            }
            self.main.window.makeKeyAndOrderFront(None);
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
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
