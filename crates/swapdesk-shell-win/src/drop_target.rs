//! Files dragged from Explorer into the viewer window (FILES-UI.md 4.2, slice 4): an OLE drop target.
//!
//! The thread is initialised with `OleInitialize` (app.rs), which `RegisterDragDrop` needs. The COM object only
//! translates: it reads the paths of `CF_HDROP` from the data object (no file-system call: whether a path is a
//! folder is read by the backend's worker after the drop), converts the screen point to the window's DIPs and calls
//! the window, which asks the UI state what the drop would do. Nothing here waits: a call ends with the effect.
//! Data without `CF_HDROP` (`CFSTR_FILEDESCRIPTOR` of a mail client or a zip folder) is reported as not files.

use std::path::PathBuf;
use std::rc::Weak;

use swapdesk_ui::{DropEffect, SystemDragData};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::core::Ref;
use windows_core::implement;

use crate::window::Window;

/// The drop target of one viewer window. Every call arrives on the UI thread (a single-threaded apartment).
#[implement(IDropTarget)]
struct DropTarget {
    window: Weak<Window>,
}

fn effect(e: DropEffect) -> DROPEFFECT {
    match e {
        DropEffect::Copy => DROPEFFECT_COPY,
        DropEffect::None => DROPEFFECT_NONE,
    }
}

/// What the data object carries: the paths of `CF_HDROP`, or "not files".
fn read(data: Option<&IDataObject>) -> SystemDragData {
    let Some(data) = data else { return SystemDragData::NotFiles };
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: GetData hands over a medium we release below; the HDROP is only read while the medium lives.
    unsafe {
        let Ok(mut medium) = data.GetData(&format) else { return SystemDragData::NotFiles };
        let drop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(drop, u32::MAX, None);
        let mut items = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(drop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
            items.push(PathBuf::from(String::from_utf16_lossy(&buf[..got.min(len)])));
        }
        ReleaseStgMedium(&mut medium);
        SystemDragData::Files(items)
    }
}

/// Writes the effect through OLE's pointer.
fn answer(out: *mut DROPEFFECT, e: DropEffect) {
    if !out.is_null() {
        // SAFETY: OLE passes a valid pointer for the effect.
        unsafe { *out = effect(e) };
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        out: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let e = match self.window.upgrade() {
            Some(w) => w.system_drag_enter(read(data.as_ref()), (pt.x, pt.y)),
            None => DropEffect::None,
        };
        answer(out, e);
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, out: *mut DROPEFFECT) -> windows::core::Result<()> {
        let e = match self.window.upgrade() {
            Some(w) => w.system_drag_over((pt.x, pt.y)),
            None => DropEffect::None,
        };
        answer(out, e);
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Some(w) = self.window.upgrade() {
            w.system_drag_leave();
        }
        Ok(())
    }

    fn Drop(
        &self,
        _data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        out: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // The paths were read at DragEnter and kept by the state.
        let e = match self.window.upgrade() {
            Some(w) => w.system_drop((pt.x, pt.y)),
            None => DropEffect::None,
        };
        answer(out, e);
        Ok(())
    }
}

/// Registers the window as a drop target. A failure (the thread is not an OLE apartment) leaves the window
/// without drops, which is harmless.
pub fn register(hwnd: HWND, window: Weak<Window>) -> bool {
    let target: IDropTarget = DropTarget { window }.into();
    // SAFETY: our own window, on the thread that runs its messages; RegisterDragDrop keeps its own reference.
    unsafe { RegisterDragDrop(hwnd, &target).is_ok() }
}

/// Removes the registration; called before the window is destroyed.
pub fn revoke(hwnd: HWND) {
    // SAFETY: our own window; a window that was never registered just returns an error.
    let _ = unsafe { RevokeDragDrop(hwnd) };
}
