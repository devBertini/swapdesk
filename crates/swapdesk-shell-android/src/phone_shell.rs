//! The phone's shell: what the Windows and Mac shells do around the shared state, without a line of Android in it.
//! It holds the state, the backend and the window; a finger's press, release and drag come in as calls, the state's
//! commands go to the backend, and what only Android can do (a list of choices, the keyboard in a text field, a page
//! in the browser) goes out as [`Ask`]s for the Java side to run.
//!
//! Gestures are Android's to recognise (its own slop, its own fling): Java tells this shell what the finger did, not
//! where it moved pixel by pixel.

use std::sync::Arc;

use swapdesk_paint::input::hit_touch;
use swapdesk_paint::{Interaction, Output};
use swapdesk_ui::state::openable_page;
use swapdesk_ui::tokens::text;
use swapdesk_ui::{
    Activation, Changed, Command, ControlId, EditId, Nav, Screen, SelectId, UiBackend, UiState, WindowKind,
};

use crate::canvas_ops::{FRAME, Writer, argb};
use crate::phone_effects::{Handling, handling};
use crate::phone_window::{FrameCost, PhoneWindow, Surface};
use crate::recorder::Metrics;

/// Something only Android can do, asked of the Java side ([`PhoneShell::take_asks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Show these choices as a list over the screen, the chosen one marked; the answer comes back through
    /// [`PhoneShell::chose`].
    Menu { items: Vec<String>, selected: Option<usize> },
    /// Put the keyboard in this text field.
    FocusEdit(EditId),
    /// Hand this field's text to [`PhoneShell::submit_edit`]; `clear` empties the field afterwards (a message sent).
    SubmitEdit { edit: EditId, clear: bool },
    /// Open this page in the phone's browser.
    OpenPage(String),
    /// Something the phone does not do yet, for the app's log.
    Note(String),
}

/// The codes of [`Ask`]s in [`PhoneShell::take_asks`].
pub mod ask {
    pub const MENU: u8 = 1;
    pub const FOCUS_EDIT: u8 = 2;
    pub const SUBMIT_EDIT: u8 = 3;
    pub const OPEN_PAGE: u8 = 4;
    pub const NOTE: u8 = 5;
    /// No choice of a menu is marked.
    pub const NONE_SELECTED: u16 = u16::MAX;
}

/// The code of a text field across the JNI boundary.
pub fn edit_code(id: EditId) -> u8 {
    match id {
        EditId::Name => 1,
        EditId::Chat => 2,
        EditId::Rename => 3,
        EditId::Address => 4,
        EditId::ExpRename => 5,
    }
}

/// The text field of a code; `None` for one this build does not know.
pub fn edit_of(code: u8) -> Option<EditId> {
    [EditId::Name, EditId::Chat, EditId::Rename, EditId::Address, EditId::ExpRename]
        .into_iter()
        .find(|e| edit_code(*e) == code)
}

pub struct PhoneShell {
    state: UiState,
    backend: Arc<dyn UiBackend>,
    window: PhoneWindow,
    ix: Interaction,
    /// Device pixels per DIP of the last frame.
    density: f32,
    /// The select whose choices are on the screen.
    menu: Option<SelectId>,
    asks: Vec<Ask>,
    /// The screen no longer shows the state: a frame is wanted.
    dirty: bool,
    /// How many commands went to the backend.
    sent: u64,
}

impl PhoneShell {
    pub fn new(state: UiState, backend: Arc<dyn UiBackend>) -> Self {
        Self {
            state,
            backend,
            window: PhoneWindow::new(),
            ix: Interaction { touch: true, ..Interaction::default() },
            density: 1.0,
            menu: None,
            asks: Vec::new(),
            dirty: true,
            sent: 0,
        }
    }

    pub fn state(&self) -> &UiState {
        &self.state
    }

    pub fn interaction(&self) -> &Interaction {
        &self.ix
    }

    /// What the last frame registered.
    pub fn output(&self) -> Option<&Output> {
        self.window.output()
    }

    pub fn cost(&self) -> FrameCost {
        self.window.cost()
    }

    /// Device pixels per DIP of the last frame.
    pub fn density(&self) -> f32 {
        self.density
    }

    /// How many commands this shell has sent to the backend.
    pub fn commands_sent(&self) -> u64 {
        self.sent
    }

    /// Whether the screen is behind the state. A frame painted clears it.
    pub fn wants_frame(&self) -> bool {
        self.dirty
    }

    /// Sends the commands a shell sends when it starts (R-UI-6).
    pub fn start(&mut self) {
        let commands = self.state.startup_commands();
        self.send(commands);
    }

    // ----- painting -----

    /// One pass of a frame of the main window for a view of this size in device pixels: the frame to replay, or
    /// the list of text to measure first (see [`PhoneWindow::paint`]).
    pub fn paint(&mut self, metrics: Metrics, width_px: f32, height_px: f32, density: f32) -> Vec<u8> {
        self.density = density;
        let on = Surface {
            kind: WindowKind::Main,
            size: (width_px / density, height_px / density),
            scale: density,
            offscreen: false,
        };
        let bytes = self.window.paint(metrics, &self.state, &self.ix, on);
        if bytes.first() == Some(&FRAME) {
            self.dirty = false;
            // As the desktop canvases do after a paint: another scrolling content starts at its top, and an offset
            // past a content that got shorter comes back to its end.
            if let Some(out) = self.window.output() {
                if self.ix.scroll_key != out.scroll_key {
                    self.ix.scroll_key = out.scroll_key;
                    self.ix.scroll = 0.0;
                }
                self.ix.scroll = self.ix.scroll.clamp(0.0, out.scroll_max);
            }
        }
        bytes
    }

    /// Java's answer to the last list of text to measure.
    pub fn measured(&mut self, answer: &[u8]) -> Result<usize, &'static str> {
        self.window.measured(answer)
    }

    // ----- a finger -----

    fn hit(&self, x_px: f32, y_px: f32) -> Option<ControlId> {
        let out = self.window.output()?;
        hit_touch(&out.widgets, x_px / self.density, y_px / self.density)
    }

    /// A finger is down at this point and stays: the control under it shows as pressed.
    pub fn press(&mut self, x_px: f32, y_px: f32) {
        let hit = self.hit(x_px, y_px);
        if self.ix.pressed != hit || self.ix.hover != hit {
            self.dirty = true;
        }
        self.ix.pressed = hit;
        self.ix.hover = hit;
        if hit.is_some() {
            self.ix.focus = hit;
            self.ix.focus_visible = false;
        }
    }

    /// The finger came up at this point without having moved away: a tap. The control that was pressed, when the
    /// finger is still on it, is activated; a text field takes the keyboard instead.
    pub fn release(&mut self, x_px: f32, y_px: f32) {
        let hit = self.hit(x_px, y_px);
        let pressed = self.ix.pressed.take();
        self.ix.hover = None;
        self.dirty = true;
        match (pressed, hit) {
            (Some(ControlId::Edit(edit)), Some(h)) if h == ControlId::Edit(edit) => {
                self.asks.push(Ask::FocusEdit(edit))
            }
            (Some(p), Some(h)) if p == h => self.activate(p),
            _ => {}
        }
    }

    /// The touch became something else (a drag that scrolls) or was taken away: nothing stays pressed.
    pub fn cancel(&mut self) {
        if self.ix.pressed.is_some() || self.ix.hover.is_some() {
            self.dirty = true;
        }
        self.ix.pressed = None;
        self.ix.hover = None;
    }

    /// Whether a drag that starts at this point moves the pane: it is inside the part that scrolls, and the pane is
    /// taller than it.
    pub fn scrolls_at(&self, x_px: f32, y_px: f32) -> bool {
        self.window.output().is_some_and(|out| {
            out.scroll_max > 0.0 && out.page_view.is_some_and(|v| v.contains(x_px / self.density, y_px / self.density))
        })
    }

    /// The pane's offset and its largest offset, in device pixels: where a fling starts and where it may end.
    pub fn scroll_range(&self) -> (f32, f32) {
        let max = self.window.output().map_or(0.0, |out| out.scroll_max);
        (self.ix.scroll.clamp(0.0, max) * self.density, max * self.density)
    }

    /// Moves the pane by `dy_px` device pixels (a finger moving up scrolls down). Returns whether it moved.
    pub fn scroll_by(&mut self, dy_px: f32) -> bool {
        self.scroll_to(self.ix.scroll * self.density + dy_px)
    }

    /// Puts the pane at `y_px` device pixels from its top, inside its range. Returns whether it moved.
    pub fn scroll_to(&mut self, y_px: f32) -> bool {
        let Some(out) = self.window.output() else { return false };
        let to = (y_px / self.density).clamp(0.0, out.scroll_max);
        self.ix.scroll_key = out.scroll_key;
        let moved = to != self.ix.scroll;
        self.ix.scroll = to;
        self.dirty |= moved;
        moved
    }

    /// Scrolls the pane so that a text field is in sight with some room around it (the keyboard took part of the
    /// screen). Returns whether the pane moved.
    pub fn reveal(&mut self, edit: EditId) -> bool {
        const ROOM: f32 = 24.0;
        let Some(out) = self.window.output() else { return false };
        let (Some(view), Some(slot)) = (out.page_view, out.edits.iter().find(|s| s.id == edit)) else { return false };
        let below = slot.rect.bottom() + ROOM - view.bottom();
        let above = view.y + ROOM - slot.rect.y;
        let by = if below > 0.0 { below } else { -above.max(0.0) };
        by != 0.0 && self.scroll_by(by * self.density)
    }

    // ----- the way back -----

    /// The first pane is on the screen: Início, or a computer's page with no computer to show.
    fn at_home(&self) -> bool {
        match self.state.nav() {
            Nav::Home => true,
            Nav::Devices => self.state.selected_device().is_none(),
            Nav::Activity | Nav::Settings => false,
        }
    }

    /// Something stands over the pane and asks for an answer: a dialog, a request to connect, the pairing screen.
    fn asked(&self) -> bool {
        self.state.dialog(WindowKind::Main).is_some()
            || self.state.incoming_session_open()
            || self.state.screen() == Screen::Pairing
    }

    /// Whether Android's back gesture is this app's to answer now. On the first pane with nothing over it, it is the
    /// system's: the app goes to the background.
    pub fn takes_back(&self) -> bool {
        self.state.renaming().is_some() || self.asked() || !self.at_home()
    }

    /// Android's back gesture, by the rule Esc follows on a desktop, then the phone's own: a name being typed is
    /// dropped; a card that waits is cancelled (`UiState::escape_target`); a question over the pane takes its way
    /// out when it has one that only leaves (cancel, close, the lesser button of a dialog) and otherwise stays,
    /// since comparing codes and a request to connect need a chosen answer; any pane but the first goes back to the
    /// first. Returns false when the gesture is the system's ([`Self::takes_back`]).
    pub fn back(&mut self) -> bool {
        if self.state.renaming().is_some() {
            self.activate(ControlId::RenameCancel);
            return true;
        }
        if let Some(id) = self.state.escape_target(WindowKind::Main) {
            self.activate(id);
            return true;
        }
        if self.asked() {
            let offered = self.state.controls(WindowKind::Main);
            let way_out =
                [ControlId::DialogSecondary, ControlId::PairCancel, ControlId::PairClose, ControlId::PairDone]
                    .into_iter()
                    .find(|id| offered.iter().any(|c| c.id == *id && !c.disabled));
            if let Some(id) = way_out {
                self.activate(id);
            }
            return true;
        }
        if !self.at_home() {
            self.activate(ControlId::NavHome);
            return true;
        }
        false
    }

    // ----- the state and the backend -----

    /// A second went by.
    pub fn tick(&mut self) {
        let changed = self.state.tick();
        self.note_change(changed);
    }

    /// The backend has news (the waker fired): they are applied, and what they ask to send is sent.
    pub fn drain(&mut self) {
        let mut changed = Changed::NONE;
        for event in self.backend.drain() {
            changed = changed.union(self.state.apply(event));
        }
        let mut followups = self.state.take_followups();
        followups.extend(self.state.take_display_request());
        self.send(followups);
        if let Some(id) = self.state.take_focus_request() {
            self.ix.focus = Some(id);
        }
        self.note_change(changed);
    }

    /// The phone's time, for what the screens say about time ("Hoje, 15:25").
    pub fn set_clock(&mut self, now_ms: u64, utc_offset_min: i32) {
        self.state.set_clock(swapdesk_ui::format::Clock { now_ms, utc_offset_min });
    }

    /// The system's light or dark appearance.
    pub fn set_dark(&mut self, dark: bool) {
        let changed = self.state.set_dark(dark);
        self.note_change(changed);
    }

    /// The text of a native field, when it acts: Enter or the field's own action, the address field's pause after
    /// the last key, or an [`Ask::SubmitEdit`].
    pub fn submit_edit(&mut self, edit: EditId, text: String) {
        let act = self.state.submit_edit(edit, text);
        self.dispatch(act);
    }

    /// The answer to an [`Ask::Menu`]: the choice picked, or `None` when the list was closed without one.
    pub fn chose(&mut self, index: Option<usize>) {
        if let (Some(select), Some(index)) = (self.menu.take(), index) {
            let act = self.state.choose(select, index);
            self.dispatch(act);
        }
    }

    fn note_change(&mut self, changed: Changed) {
        // One window shows everything a phone shows.
        if !changed.is_none() {
            self.dirty = true;
        }
    }

    fn send(&mut self, commands: Vec<Command>) {
        for command in commands {
            let ticket = self.backend.send(command.clone());
            self.state.track(ticket, &command);
            self.sent += 1;
        }
    }

    fn activate(&mut self, id: ControlId) {
        let act = self.state.activate(id);
        self.dispatch(act);
    }

    fn dispatch(&mut self, act: Activation) {
        self.send(act.commands);
        self.note_change(act.changed);
        if let Some(effect) = act.effect {
            self.run(handling(effect));
        }
    }

    fn run(&mut self, handling: Handling) {
        match handling {
            Handling::Menu(select) => {
                let options = self.state.select_options(select);
                let selected = options.iter().position(|(_, on)| *on);
                self.menu = Some(select);
                self.asks.push(Ask::Menu { items: options.into_iter().map(|(name, _)| name).collect(), selected });
            }
            Handling::Focus(edit) => self.asks.push(Ask::FocusEdit(edit)),
            Handling::FocusControl(id) => {
                self.ix.focus = Some(id);
                self.dirty = true;
            }
            Handling::Submit(edit) => self.asks.push(Ask::SubmitEdit { edit, clear: edit == EditId::Chat }),
            Handling::OpenWebPage => {
                if let Some(page) = self.state.take_web_page().filter(|p| openable_page(p) && p.starts_with("https://"))
                {
                    self.asks.push(Ask::OpenPage(page));
                }
            }
            Handling::Later(what) => self.asks.push(Ask::Note(format!("not built yet: {what}"))),
            Handling::NotOnAPhone(what) => self.asks.push(Ask::Note(format!("not on a phone: {what}"))),
        }
    }

    // ----- what Java reads -----

    /// The asks queued since the last call, for the Java side, every number little-endian:
    ///
    /// ```text
    /// u8 count, then per ask: u8 code ([`ask`]) and
    ///   1 menu:        u16 selected (0xFFFF: none), u16 count, the choices (each u16 length, UTF-8)
    ///   2 focus edit:  u8 field ([`edit_code`])
    ///   3 submit edit: u8 field, u8 clear (0 or 1)
    ///   4 open page:   the address (u16 length, UTF-8)
    ///   5 note:        the text (u16 length, UTF-8)
    /// ```
    pub fn take_asks(&mut self) -> Vec<u8> {
        let asks = std::mem::take(&mut self.asks);
        let asks = &asks[..asks.len().min(usize::from(u8::MAX))];
        let mut w = Writer::default();
        w.u8(asks.len() as u8);
        for a in asks {
            match a {
                Ask::Menu { items, selected } => {
                    let items = &items[..items.len().min(usize::from(ask::NONE_SELECTED) - 1)];
                    w.u8(ask::MENU);
                    w.u16(selected.filter(|s| *s < items.len()).map_or(ask::NONE_SELECTED, |s| s as u16));
                    w.u16(items.len() as u16);
                    for item in items {
                        w.str(item);
                    }
                }
                Ask::FocusEdit(edit) => {
                    w.u8(ask::FOCUS_EDIT);
                    w.u8(edit_code(*edit));
                }
                Ask::SubmitEdit { edit, clear } => {
                    w.u8(ask::SUBMIT_EDIT);
                    w.u8(edit_code(*edit));
                    w.u8(u8::from(*clear));
                }
                Ask::OpenPage(page) => {
                    w.u8(ask::OPEN_PAGE);
                    w.str(page);
                }
                Ask::Note(text) => {
                    w.u8(ask::NOTE);
                    w.str(text);
                }
            }
        }
        w.0
    }

    /// The asks queued, without taking them (the tests).
    pub fn asks(&self) -> &[Ask] {
        &self.asks
    }

    /// The native text fields of the last frame, for the Java side to put over the drawn ones, in device pixels,
    /// every number little-endian:
    ///
    /// ```text
    /// f32 x, y, width, height   the part of the view the fields show in (they are cut at its edges): the pane
    ///                           under the bar, or the whole view when nothing scrolls
    /// f32 text size, u32 ink, u32 placeholder ink (0xAARRGGBB)
    /// u8 count, then per field: u8 field ([`edit_code`]), f32 x, y, width, height (its text area),
    ///                           its value and its placeholder (each u16 length, UTF-8)
    /// ```
    pub fn edits(&self) -> Vec<u8> {
        let d = self.density;
        let mut w = Writer::default();
        let pal = self.state.palette();
        let out = self.window.output();
        match out.and_then(|o| o.page_view) {
            Some(v) => w.floats(&[v.x * d, v.y * d, v.w * d, v.h * d]),
            None => w.floats(&[0.0, 0.0, f32::MAX, f32::MAX]),
        }
        w.f32(text::BODY.size * d);
        w.u32(argb(pal.ink));
        w.u32(argb(pal.soft));
        let slots = out.map_or(&[][..], |o| &o.edits[..]);
        let slots = &slots[..slots.len().min(usize::from(u8::MAX))];
        w.u8(slots.len() as u8);
        for slot in slots {
            w.u8(edit_code(slot.id));
            w.floats(&[slot.rect.x * d, slot.rect.y * d, slot.rect.w * d, slot.rect.h * d]);
            w.str(&slot.value);
            w.str(&slot.placeholder);
        }
        w.0
    }
}
