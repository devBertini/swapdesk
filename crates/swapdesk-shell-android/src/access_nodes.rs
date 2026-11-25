//! What a screen reader finds on the phone's screen (docs/ANDROID.md 8.5, D92): the shared tree of the window
//! (`UiState::controls`, the list Windows hands to UI Automation and the Mac to AppKit) placed where the last frame
//! drew it, as a flat list of nodes for the Java view's `AccessibilityNodeProvider`.
//!
//! Rust never calls Java for it: after each frame the view reads the list as bytes ([`encode`]) and answers
//! Android's questions from it; an action on a node comes back as a call ([`crate::PhoneShell::access_action`]).
//!
//! * A control is where its widget is, cut to what shows of it (its touch rectangle is already cut to the page
//!   it scrolls in): a control scrolled out of sight is not listed, and the pane says it can scroll instead.
//! * A text or a heading is where the frame drew a label of that text: the desktops give such nodes no place,
//!   a phone's reader needs one (TalkBack skips a node with no bounds). Labels are taken in the order they were
//!   drawn, each once; a text the frame did not draw is not listed (counted by [`Built::unplaced`]).
//! * A text field is the native field over the drawn one: it is listed by its field, so the view puts the real
//!   `EditText` at its place in the order, labelled, and never a twin of it.
//! * A dialog, a request to connect or the pairing screen stands over the pane: only its own nodes are listed,
//!   as a modal window of a desktop lets nothing behind it be reached.

use std::collections::HashMap;

use swapdesk_paint::{Output, Rect};
use swapdesk_ui::{ControlId, ControlSpec, Region, UiRole};

use crate::canvas_ops::Writer;
use crate::phone_shell::edit_code;

/// A label the last frame drew: its whole text (before any trim with an ellipsis) and the part of it that shows,
/// in DIPs (empty when it is clipped away).
#[derive(Debug, Clone, PartialEq)]
pub struct Drawn {
    pub text: String,
    pub rect: Rect,
}

/// The kind of a node, which the view turns into an Android class name.
pub mod kind {
    pub const BUTTON: u8 = 1;
    pub const TAB: u8 = 2;
    pub const SWITCH: u8 = 3;
    pub const CHECK_BOX: u8 = 4;
    pub const LINK: u8 = 5;
    pub const COMBO_BOX: u8 = 6;
    /// The native text field of [`super::Node::edit`].
    pub const EDIT: u8 = 7;
    pub const HEADING: u8 = 8;
    pub const TEXT: u8 = 9;
    pub const PROGRESS: u8 = 10;
    pub const TABLE: u8 = 11;
    pub const ROW: u8 = 12;
    pub const CELL: u8 = 13;
    pub const COLUMN_HEADER: u8 = 14;
}

/// What a node can do and is, as bits.
pub mod flag {
    pub const CLICKABLE: u16 = 1;
    pub const CHECKABLE: u16 = 1 << 1;
    pub const CHECKED: u16 = 1 << 2;
    pub const SELECTED: u16 = 1 << 3;
    pub const DISABLED: u16 = 1 << 4;
    /// The control has the shell's focus.
    pub const FOCUSED: u16 = 1 << 5;
}

/// What the pane can do, as bits of the list's header.
pub mod pane {
    pub const SCROLLS_BACK: u8 = 1;
    pub const SCROLLS_FORWARD: u8 = 1 << 1;
}

/// One node of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Android's virtual id: small, positive and the same for a control across frames ([`Ids`]).
    pub id: i32,
    pub control: ControlId,
    pub kind: u8,
    /// A text field's code (`phone_shell::edit_code`), 0 for any other node.
    pub edit: u8,
    pub flags: u16,
    /// In DIPs.
    pub rect: Rect,
    pub label: String,
    pub description: String,
    pub value: String,
}

/// Android's virtual ids of the controls. A node keeps its id while it is on the screen and after, so the reader's
/// focus stays on it across frames; the table starts over past [`Ids::MAX`] controls.
#[derive(Debug, Default)]
pub struct Ids {
    of: HashMap<ControlId, i32>,
    next: i32,
}

impl Ids {
    pub const MAX: usize = 4096;

    pub fn id(&mut self, control: ControlId) -> i32 {
        if self.of.len() >= Self::MAX && !self.of.contains_key(&control) {
            self.of.clear();
            self.next = 0;
        }
        let next = &mut self.next;
        *self.of.entry(control).or_insert_with(|| {
            *next += 1;
            *next
        })
    }

    /// The control of a virtual id given before.
    pub fn control(&self, id: i32) -> Option<ControlId> {
        self.of.iter().find(|(_, v)| **v == id).map(|(c, _)| *c)
    }
}

/// The list of one frame.
#[derive(Debug, Default)]
pub struct Built {
    pub nodes: Vec<Node>,
    /// Texts and headings the frame did not draw a label of, by their text (they are not listed).
    pub unplaced: Vec<String>,
}

fn kind_of(role: UiRole) -> u8 {
    match role {
        UiRole::Button => kind::BUTTON,
        UiRole::Tab => kind::TAB,
        UiRole::Switch => kind::SWITCH,
        UiRole::CheckBox => kind::CHECK_BOX,
        UiRole::Link => kind::LINK,
        UiRole::ComboBox => kind::COMBO_BOX,
        UiRole::TextInput => kind::EDIT,
        UiRole::Heading => kind::HEADING,
        UiRole::Text => kind::TEXT,
        UiRole::Progress => kind::PROGRESS,
        UiRole::Table => kind::TABLE,
        UiRole::Row => kind::ROW,
        UiRole::Cell => kind::CELL,
        UiRole::ColumnHeader => kind::COLUMN_HEADER,
    }
}

/// The labels a text of the tree was drawn as: one label of the same text, else the first run of consecutive
/// labels not taken yet, each a part of the text, that together make up at least half of its letters. A row of a
/// list is one text in the tree ("Hoje, 15:25, MacBook, Controlou..., 17 min") and several labels on the screen in
/// another order; a code is "Código 482 913" in the tree and "482 913" under its heading.
fn place(text: &str, drawn: &[Drawn], taken: &[bool]) -> Option<std::ops::Range<usize>> {
    if let Some(i) = (0..drawn.len()).find(|i| !taken[*i] && drawn[*i].text == text) {
        return Some(i..i + 1);
    }
    let letters = |t: &str| t.chars().filter(|c| c.is_alphanumeric()).count();
    let whole = letters(text);
    if whole == 0 {
        return None;
    }
    let part = |i: usize| !taken[i] && letters(&drawn[i].text) > 0 && text.contains(drawn[i].text.as_str());
    let mut start = 0;
    while start < drawn.len() {
        if !part(start) {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut covered = 0;
        while end < drawn.len() && part(end) {
            covered += letters(&drawn[end].text);
            end += 1;
        }
        if covered * 2 >= whole {
            return Some(start..end);
        }
        start = end;
    }
    None
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
}

fn visible(r: Rect) -> bool {
    r.w > 0.0 && r.h > 0.0
}

/// The nodes of a frame: `specs` are the window's controls (`UiState::controls`), `out` what the frame registered,
/// `drawn` the labels it drew in their order, `focus` the shell's focused control.
pub fn build(specs: &[ControlSpec], out: &Output, drawn: &[Drawn], focus: Option<ControlId>, ids: &mut Ids) -> Built {
    let modal = specs.iter().any(|c| c.region == Region::Dialog);
    let mut taken = vec![false; drawn.len()];
    let mut built = Built::default();
    for c in specs.iter().filter(|c| !modal || c.region == Region::Dialog) {
        let (rect, edit) = match c.id {
            ControlId::Edit(e) => {
                let Some(slot) = out.edits.iter().find(|s| s.id == e) else { continue };
                // The part that shows: the slot cut to its widget's touch rectangle, which is cut to the page.
                let shown =
                    out.widgets.iter().find(|w| w.id == c.id).map_or(slot.rect, |w| slot.rect.intersect(w.touch));
                (shown, edit_code(e))
            }
            ControlId::Static(_) => {
                let Some(run) = place(&c.label, drawn, &taken) else {
                    built.unplaced.push(c.label.clone());
                    continue;
                };
                let mut rect: Option<Rect> = None;
                for i in run {
                    taken[i] = true;
                    let r = drawn[i].rect;
                    if visible(r) {
                        rect = Some(rect.map_or(r, |u| union(u, r)));
                    }
                }
                (rect.unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0)), 0)
            }
            id => {
                let Some(w) = out.widgets.iter().find(|w| w.id == id) else { continue };
                (w.rect.intersect(w.touch), 0)
            }
        };
        if !visible(rect) {
            continue;
        }
        let mut flags = 0;
        if c.id.focusable() && !matches!(c.id, ControlId::Edit(_)) {
            flags |= flag::CLICKABLE;
        }
        if let Some(on) = c.toggled {
            flags |= flag::CHECKABLE;
            if on {
                flags |= flag::CHECKED;
            }
        }
        if c.selected {
            flags |= flag::SELECTED;
        }
        if c.disabled {
            flags |= flag::DISABLED;
        }
        if focus == Some(c.id) {
            flags |= flag::FOCUSED;
        }
        built.nodes.push(Node {
            id: ids.id(c.id),
            control: c.id,
            kind: if edit != 0 { kind::EDIT } else { kind_of(c.role) },
            edit,
            flags,
            rect,
            label: c.label.clone(),
            description: c.description.clone().unwrap_or_default(),
            value: c.value.clone().unwrap_or_default(),
        });
    }
    built
}

/// The list for the Java view, every number little-endian, rectangles in device pixels (x, y, width, height):
///
/// ```text
/// u8 pane ([`pane`] bits: the page can scroll back, forward)
/// u16 count, then per node: u32 id, u8 kind ([`kind`]), u8 field (a text field's code, else 0), u16 flags
///                           ([`flag`]), f32 x, y, width, height, its label, description and value (each u16
///                           length, UTF-8)
/// ```
pub fn encode(nodes: &[Node], scroll: f32, scroll_max: f32, density: f32) -> Vec<u8> {
    let mut w = Writer::default();
    let mut p = 0;
    if scroll > 0.0 {
        p |= pane::SCROLLS_BACK;
    }
    if scroll < scroll_max {
        p |= pane::SCROLLS_FORWARD;
    }
    w.u8(p);
    let nodes = &nodes[..nodes.len().min(usize::from(u16::MAX))];
    w.u16(nodes.len() as u16);
    for n in nodes {
        w.u32(n.id as u32);
        w.u8(n.kind);
        w.u8(n.edit);
        w.u16(n.flags);
        w.floats(&[n.rect.x * density, n.rect.y * density, n.rect.w * density, n.rect.h * density]);
        w.str(&n.label);
        w.str(&n.description);
        w.str(&n.value);
    }
    w.0
}
