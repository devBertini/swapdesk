//! The shell's side of the viewer pipeline (CONTRACTS.md C6: video never crosses the UI API).
//!
//! The viewer pipeline lives in `swapdesk-app`; the shell only tells it what the viewer window does, through a
//! [`ViewerHost`] given to [`crate::run_with_host`]. Every method is called on the UI thread and must return
//! quickly (queue the work, never block):
//!
//! - [`ViewerHost::opened`] once the viewer window exists and its first frame was laid out, with the raw composition
//!   pointers the renderer attaches its video visual to and the video rectangle in window pixels;
//! - [`ViewerHost::resized`] whenever the video rectangle or the DPI changes (the layout that follows `WM_SIZE` and
//!   `WM_DPICHANGED`, so the rectangle is the one the shell drew, not the window's client size);
//! - [`ViewerHost::closed`] when the window is destroyed, before its composition tree goes;
//! - focus, mouse and keyboard messages of the window, in client pixels.
//!
//! [`HeadlessSurface`] is a composition tree with no window, for tests and offscreen runs of the pipeline.

use swapdesk_platform::{NativeSurface, Rect};
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows::core::{Interface, Result};

use crate::surface::Gfx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Button {
        button: PointerButton,
        down: bool,
    },
    /// In 1/120 of a notch; `dy > 0` is away from the user, `dx > 0` right.
    Wheel {
        dx: i16,
        dy: i16,
    },
}

/// A mouse message of the viewer window. `x`, `y` are client pixels; `inside` says the point is in the video
/// rectangle. The shell forwards only what is over the video (or follows a press that began there), never a click on
/// the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMsg {
    pub kind: MouseKind,
    pub x: i32,
    pub y: i32,
    pub inside: bool,
}

/// A key message of the viewer window, as the low-level hook would describe it: virtual key, 8-bit scan code and the
/// extended flag. A message with `vk == 0` and `down` carries text only (an IME's composed `WM_CHAR`s): no key is
/// pressed, `text` is what was typed. `text` and `dead` are what the viewer's layout made of the press (from the `WM_CHAR` and
/// `WM_DEADCHAR` messages that follow it; empty for key-ups, shortcuts and keys with no text). `locks` is the toggle
/// state: bit 0 Caps, 1 Num, 2 Scroll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMsg {
    pub vk: u32,
    pub scan: u32,
    pub extended: bool,
    pub down: bool,
    pub text: Option<String>,
    pub dead: bool,
    pub locks: u8,
}

/// What the viewer window reports to the viewer pipeline. See the module documentation.
pub trait ViewerHost: Send + Sync {
    /// The viewer window is up. `scale` is the DPI scale (1.0 at 96 dpi); `area` is in window pixels.
    fn opened(&self, surface: NativeSurface, area: Rect, scale: f32);
    fn resized(&self, area: Rect, scale: f32);
    /// The window is being destroyed: detach everything that was built on `opened`'s pointers before returning.
    fn closed(&self);
    fn focus(&self, focused: bool);
    /// True when the message was taken (the shell then does not give it to the UI).
    fn mouse(&self, msg: MouseMsg) -> bool;
    /// True while the pipeline wants the window's key messages (the shell then reads `WM_CHAR`s and asks `key`).
    fn captures_keyboard(&self) -> bool;
    /// True when the key was taken (forwarded, or left to the system-wide grabber); the shell then keeps it from the UI.
    fn key(&self, msg: &KeyMsg) -> bool;
    /// The host's cursor shape while the viewer controls the host: the local pointer takes it over the video, and the
    /// remote arrow is not drawn (it would trail the pointer by a round trip). `None`: the system arrow.
    fn pointer_shape(&self) -> Option<std::sync::Arc<swapdesk_platform::CursorShape>>;
    /// The viewer's own shortcuts (D30) that fired since the last call, oldest first; the shell runs them on the state.
    /// The pipeline wakes the UI thread (the shell's waker) when one is queued.
    fn take_shortcuts(&self) -> Vec<swapdesk_ui::ViewerAction> {
        Vec::new()
    }
    /// True once each time the shell's graphics device was found gone (F-VIEW-08): the shell builds its device and its
    /// windows' surfaces again and hands the new surface with `closed` then `opened`. The pipeline wakes the UI thread
    /// when it turns true.
    fn device_lost(&self) -> bool {
        false
    }
}

/// A composition tree with a D3D11 device and a parent visual, and no window: what the renderer needs from a shell,
/// for tests and offscreen runs. Nothing is composed to a screen.
pub struct HeadlessSurface {
    gfx: Gfx,
    _root: IDCompositionVisual,
    parent: IDCompositionVisual,
}

impl HeadlessSurface {
    pub fn new() -> Result<Self> {
        let gfx = Gfx::new()?;
        // SAFETY: plain creation of composition objects on a live device.
        let (root, parent) = unsafe {
            let root = gfx.dcomp.CreateVisual()?;
            let parent = gfx.dcomp.CreateVisual()?;
            root.AddVisual(&parent, false, None::<&IDCompositionVisual>)?;
            gfx.dcomp.Commit()?;
            (root, parent)
        };
        Ok(HeadlessSurface { gfx, _root: root, parent })
    }

    /// The raw pointers a renderer attaches with (the same three a real window hands out).
    pub fn native(&self) -> NativeSurface {
        NativeSurface::Windows {
            dcomp_device: self.gfx.dcomp.as_raw() as usize as u64,
            parent_visual: self.parent.as_raw() as usize as u64,
            d3d11_device: self.gfx.d3d.as_raw() as usize as u64,
        }
    }
}
