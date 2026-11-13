//! Slice 5 (`docs/FILES-UI.md` 4.3): the files of the other computer dragged out of the explorer's right pane into
//! Explorer or onto the desktop.
//!
//! An OLE drag whose data object offers `CFSTR_FILEDESCRIPTORW` (name, size, time, `FD_PROGRESSUI`) and
//! `CFSTR_FILECONTENTS` per file as an `IStream`. The data object also answers `IDataObjectAsyncCapability`, so
//! Explorer extracts on a background thread and `DoDragDrop` returns at once.
//!
//! COM delivers calls on the thread of the apartment that made the object (the UI thread, an STA) unless the caller
//! shares the process. So a stream's `Read` never works on the thread it is called on when that is the UI thread: it
//! hands the blocking part (wait for the bytes, read the staging file) to a worker and pumps messages meanwhile, so
//! the transfer's own events still reach the state. No file-system call runs on the UI thread.

use std::ffi::c_void;
use std::io::{Read, Seek, SeekFrom};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use swapdesk_ui::{Avail, DragOut, DragOutFile, PullError};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_TYMED,
    E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, FILETIME, HWND, LPARAM, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK,
    STG_E_INVALIDFUNCTION, STG_E_READFAULT, WPARAM,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource, IDropSource_Impl};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{
    FILEDESCRIPTORW, IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetClientRect, MSG, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, TranslateMessage,
    WM_APP, WM_QUIT,
};
use windows::core::{BOOL, HRESULT, Ref, w};
use windows_core::implement;

use crate::app::Shared;
use crate::window::Window;

/// Posted to the main window by a drag out's waker: call [`Shared::system_drag_pump`].
pub const WM_SYSDRAG: u32 = WM_APP + 60;

const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_WRITESTIME: u32 = 0x0000_0020;
const FD_FILESIZE: u32 = 0x0000_0040;
const FD_PROGRESSUI: u32 = 0x0000_4000;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// The pointer (client pixels `x`, `y`) is outside the client area of `hwnd`.
pub fn outside_client(hwnd: HWND, x: i32, y: i32) -> bool {
    let mut r = windows::Win32::Foundation::RECT::default();
    // SAFETY: a valid window and an out struct.
    if unsafe { GetClientRect(hwnd, &mut r) }.is_err() {
        return false;
    }
    x < r.left || y < r.top || x >= r.right || y >= r.bottom
}

impl Shared {
    /// The pointer left the session window with the left button held (slice 5): a drag of the right pane becomes a
    /// system drag (the effect starts it once the message has finished), anything else changes nothing.
    pub fn drag_left_window(self: &Rc<Self>, from: &Window) {
        let act = self.state.borrow_mut().drag_left_window();
        self.dispatch(act, from);
    }

    /// A drag out's waker fired: the downloads the system asked for start, a released drag's transfers cancel.
    pub fn system_drag_pump(self: &Rc<Self>) {
        let act = self.state.borrow_mut().system_drag_pump();
        self.send_commands(act.commands);
    }

    /// Runs the OLE drag of the drag [`swapdesk_ui::UiState::take_system_drag`] holds. Called outside the mouse
    /// message (the effect is queued), because `DoDragDrop` runs a modal loop.
    pub fn run_system_drag(self: &Rc<Self>, from: &Window) {
        let Some(out) = self.state.borrow_mut().take_system_drag() else { return };
        let hwnd = from.hwnd().0 as isize;
        out.set_waker(Arc::new(move || {
            // SAFETY: posting to a window handle is safe from any thread; a dead handle only fails.
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut c_void)), WM_SYSDRAG, WPARAM(0), LPARAM(0));
            }
        }));
        let Some(data) = make_data_object(out) else { return };
        let source = IDropSource::from(DropSource);
        let mut effect = DROPEFFECT(0);
        // SAFETY: DoDragDrop needs the UI thread's OLE initialisation, which the shell does at startup; the mouse
        // capture of the in-app drag is let go first, the loop takes its own.
        unsafe {
            let _ = ReleaseCapture();
            let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        }
        // The data object may live on: Explorer extracts on its own thread. It tells the drag out to release
        // when its last reference (and its streams') goes.
    }
}

fn clipboard_format(name: windows::core::PCWSTR) -> u16 {
    // SAFETY: a static NUL-terminated name.
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// What the last reference of a drag out's objects does: lets the drag out go.
struct ReleaseOnDrop(Arc<DragOut>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject {
    out: Arc<DragOut>,
    guard: Arc<ReleaseOnDrop>,
    cf_descriptor: u16,
    cf_contents: u16,
    cf_effect: u16,
    ui_thread: u32,
    async_mode: AtomicBool,
    in_operation: AtomicBool,
}

fn make_data_object(out: Arc<DragOut>) -> Option<IDataObject> {
    let obj = DataObject {
        guard: Arc::new(ReleaseOnDrop(out.clone())),
        out,
        cf_descriptor: clipboard_format(w!("FileGroupDescriptorW")),
        cf_contents: clipboard_format(w!("FileContents")),
        cf_effect: clipboard_format(w!("Preferred DropEffect")),
        // SAFETY: plain query.
        ui_thread: unsafe { GetCurrentThreadId() },
        async_mode: AtomicBool::new(true),
        in_operation: AtomicBool::new(false),
    };
    Some(IDataObject::from(obj))
}

/// `FILETIME` of milliseconds since the Unix epoch.
fn filetime(ms: u64) -> FILETIME {
    let t = ms.saturating_mul(10_000).saturating_add(116_444_736_000_000_000);
    FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 }
}

/// A name Explorer accepts: the characters Windows forbids become `_`, and it is cut to the descriptor's size.
pub(crate) fn descriptor_name(name: &str) -> [u16; 260] {
    let mut out = [0u16; 260];
    let clean: Vec<u16> = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect::<String>()
        .encode_utf16()
        .take(swapdesk_ui::MAX_NAME_UNITS)
        .collect();
    out[..clean.len()].copy_from_slice(&clean);
    out
}

fn descriptor(f: &DragOutFile) -> FILEDESCRIPTORW {
    FILEDESCRIPTORW {
        dwFlags: FD_ATTRIBUTES | FD_WRITESTIME | FD_FILESIZE | FD_PROGRESSUI,
        dwFileAttributes: FILE_ATTRIBUTE_NORMAL,
        ftLastWriteTime: filetime(f.mtime_ms),
        nFileSizeHigh: (f.size >> 32) as u32,
        nFileSizeLow: f.size as u32,
        cFileName: descriptor_name(&f.name),
        ..Default::default()
    }
}

/// A movable global block holding `bytes`.
fn global_bytes(bytes: &[u8]) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
    // SAFETY: the block is allocated, locked, filled within its size, unlocked.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

fn hglobal_medium(h: windows::Win32::Foundation::HGLOBAL) -> STGMEDIUM {
    let mut m = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, ..Default::default() };
    m.u.hGlobal = h;
    m
}

fn format(cf: u16, tymed: i32, index: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: tymed as u32,
    }
}

impl DataObject {
    fn descriptors(&self) -> windows::core::Result<STGMEDIUM> {
        let files = self.out.files();
        let each = size_of::<FILEDESCRIPTORW>();
        let mut bytes = vec![0u8; size_of::<u32>() + each * files.len()];
        bytes[..4].copy_from_slice(&(files.len() as u32).to_le_bytes());
        for (i, f) in files.iter().enumerate() {
            let d = descriptor(f);
            // SAFETY: FILEDESCRIPTORW is plain data (`repr(C, packed(1))` in the SDK): its bytes are copied at the
            // offset the group descriptor puts the i-th one.
            let src = unsafe { std::slice::from_raw_parts((&raw const d).cast::<u8>(), each) };
            let at = size_of::<u32>() + i * each;
            bytes[at..at + each].copy_from_slice(src);
        }
        Ok(hglobal_medium(global_bytes(&bytes)?))
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: COM passes a valid FORMATETC.
        let f = unsafe { pformatetcin.as_ref() }.ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        if f.cfFormat == self.cf_descriptor {
            if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            return self.descriptors();
        }
        if f.cfFormat == self.cf_effect {
            if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            return Ok(hglobal_medium(global_bytes(&DROPEFFECT_COPY.0.to_le_bytes())?));
        }
        if f.cfFormat == self.cf_contents {
            if f.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let index = usize::try_from(f.lindex).map_err(|_| windows::core::Error::from(DV_E_FORMATETC))?;
            let Some(file) = self.out.files().get(index) else { return Err(DV_E_FORMATETC.into()) };
            // The download starts now: the staging folder is made on a worker, the UI thread is woken to send it.
            let _ = self.out.request(index);
            let stream = FileStream {
                out: self.out.clone(),
                _guard: self.guard.clone(),
                index,
                size: file.size,
                pos: Mutex::new(0),
                ui_thread: self.ui_thread,
            };
            let mut m = STGMEDIUM { tymed: TYMED_ISTREAM.0 as u32, ..Default::default() };
            m.u.pstm = std::mem::ManuallyDrop::new(Some(IStream::from(stream)));
            return Ok(m);
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: COM passes a valid FORMATETC.
        let Some(f) = (unsafe { pformatetc.as_ref() }) else { return DV_E_FORMATETC };
        if f.cfFormat == self.cf_descriptor || f.cfFormat == self.cf_effect {
            return if f.tymed & TYMED_HGLOBAL.0 as u32 != 0 { S_OK } else { DV_E_TYMED };
        }
        if f.cfFormat == self.cf_contents {
            let ok = f.tymed & TYMED_ISTREAM.0 as u32 != 0
                && usize::try_from(f.lindex).is_ok_and(|i| i < self.out.files().len());
            return if ok { S_OK } else { DV_E_TYMED };
        }
        DV_E_FORMATETC
    }

    fn GetCanonicalFormatEtc(&self, pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if !pformatectin.is_null() && !pformatetcout.is_null() {
            // SAFETY: both pointers are valid FORMATETCs.
            unsafe {
                *pformatetcout = *pformatectin;
                (*pformatetcout).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats = [
            format(self.cf_descriptor, TYMED_HGLOBAL.0, -1),
            format(self.cf_contents, TYMED_ISTREAM.0, 0),
            format(self.cf_effect, TYMED_HGLOBAL.0, -1),
        ];
        // SAFETY: the array is copied by the call.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<windows::Win32::System::Com::IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[allow(non_snake_case)]
impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
        self.async_mode.store(fdoopasync.as_bool(), Ordering::Relaxed);
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.async_mode.load(Ordering::Relaxed).into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.in_operation.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows::core::Result<()> {
        self.in_operation.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[implement(IDropSource)]
struct DropSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if grfkeystate.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// The contents of one file: reads wait for the bytes and fail when the transfer fails.
#[implement(IStream)]
struct FileStream {
    out: Arc<DragOut>,
    _guard: Arc<ReleaseOnDrop>,
    index: usize,
    size: u64,
    pos: Mutex<u64>,
    ui_thread: u32,
}

/// Runs `work` where blocking is allowed: on this thread when it is not the UI thread, else on a worker while this
/// thread pumps messages (the transfer's events and the waker's wake must still be handled).
fn off_ui<T: Send + 'static>(ui_thread: u32, work: impl FnOnce() -> T + Send + 'static) -> T {
    // SAFETY: plain query.
    if unsafe { GetCurrentThreadId() } != ui_thread {
        return work();
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    loop {
        match rx.try_recv() {
            Ok(v) => return v,
            Err(mpsc::TryRecvError::Empty) => pump_messages(Duration::from_millis(15)),
            Err(mpsc::TryRecvError::Disconnected) => unreachable!("the worker sends before it ends"),
        }
    }
}

/// Dispatches the messages of this thread, then waits up to `wait` for more.
fn pump_messages(wait: Duration) {
    use windows::Win32::UI::WindowsAndMessaging::{MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, QS_ALLINPUT};
    // SAFETY: the standard message pump of a thread that owns windows.
    unsafe {
        let _ = MsgWaitForMultipleObjectsEx(None, wait.as_millis() as u32, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Reads up to `want` bytes of file `index` at `pos`, waiting for them. Runs where blocking is fine.
fn read_at(out: &DragOut, index: usize, size: u64, pos: u64, want: usize) -> Result<Vec<u8>, PullError> {
    if size == 0 {
        out.wait_done(index)?;
        return Ok(Vec::new());
    }
    for _ in 0..5 {
        let (path, limit) = match out.wait_bytes(index, pos + 1)? {
            Avail::Partial { path, limit } => (path, limit),
            Avail::Whole(path) => (path, u64::MAX),
        };
        let n = (want as u64).min(limit - pos).min(size - pos) as usize;
        let mut buf = vec![0u8; n];
        let read = std::fs::File::open(&path).and_then(|mut f| {
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(&mut buf)
        });
        match read {
            Ok(()) => return Ok(buf),
            // The partial file was renamed to its final name (or taken away) between the wait and the open.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(PullError::Failed),
        }
    }
    Err(PullError::Failed)
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        let pos = *self.pos.lock().unwrap_or_else(|e| e.into_inner());
        let set_read = |n: u32| {
            if !pcbread.is_null() {
                // SAFETY: the caller's out parameter.
                unsafe { *pcbread = n };
            }
        };
        if pos >= self.size && self.size > 0 || cb == 0 {
            set_read(0);
            return S_FALSE;
        }
        let (out, index, size) = (self.out.clone(), self.index, self.size);
        let got = off_ui(self.ui_thread, move || read_at(&out, index, size, pos, cb as usize));
        match got {
            Ok(bytes) => {
                // SAFETY: `pv` holds `cb` bytes and `bytes.len() <= cb`.
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pv.cast::<u8>(), bytes.len()) };
                *self.pos.lock().unwrap_or_else(|e| e.into_inner()) = pos + bytes.len() as u64;
                set_read(bytes.len() as u32);
                if bytes.is_empty() { S_FALSE } else { S_OK }
            }
            Err(_) => {
                set_read(0);
                STG_E_READFAULT
            }
        }
    }

    fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_INVALIDFUNCTION
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        let base = match dworigin {
            STREAM_SEEK_SET => 0i128,
            STREAM_SEEK_CUR => *pos as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let to = base + dlibmove as i128;
        if to < 0 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        *pos = to as u64;
        if !plibnewposition.is_null() {
            // SAFETY: the caller's out parameter.
            unsafe { *plibnewposition = *pos };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_FAIL.into());
        }
        // SAFETY: the caller's out parameter; no name is returned.
        unsafe {
            *pstatstg = STATSTG { r#type: STGTY_STREAM.0 as u32, cbSize: self.size, ..Default::default() };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::FILEGROUPDESCRIPTORW;

    #[test]
    fn a_descriptor_name_is_clean_and_fits_max_path() {
        let n = descriptor_name("a:b?c.txt");
        let len = n.iter().position(|c| *c == 0).unwrap();
        assert_eq!(String::from_utf16_lossy(&n[..len]), "a_b_c.txt");
        let long = descriptor_name(&"x".repeat(400));
        assert_eq!(long.iter().position(|c| *c == 0), Some(swapdesk_ui::MAX_NAME_UNITS));
    }

    #[test]
    fn the_group_descriptor_layout_is_the_sdks() {
        assert_eq!(size_of::<FILEGROUPDESCRIPTORW>(), 4 + size_of::<FILEDESCRIPTORW>());
    }

    fn staged(tag: &str, bytes: &[u8]) -> (Arc<DragOut>, IDataObject, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("swapdesk-win-dragout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), bytes).unwrap();
        let files = vec![DragOutFile { name: "a.bin".into(), size: bytes.len() as u64, mtime_ms: 1_700_000_000_000 }];
        let out = DragOut::new(1, String::new(), files, dir.clone());
        let data = make_data_object(out.clone()).unwrap();
        (out, data, dir)
    }

    fn contents(data: &IDataObject, index: i32) -> IStream {
        let f = format(clipboard_format(w!("FileContents")), TYMED_ISTREAM.0, index);
        // SAFETY: a valid FORMATETC; the medium's stream is taken out of its ManuallyDrop.
        unsafe {
            let m = data.GetData(&f).expect("the contents");
            std::mem::ManuallyDrop::into_inner(m.u.pstm).expect("a stream")
        }
    }

    fn read(stream: &IStream, cb: u32) -> (HRESULT, Vec<u8>) {
        let mut buf = vec![0u8; cb as usize];
        let mut n = 0u32;
        // SAFETY: a buffer of `cb` bytes and an out count.
        let hr = unsafe { stream.Read(buf.as_mut_ptr().cast(), cb, Some(&mut n)) };
        buf.truncate(n as usize);
        (hr, buf)
    }

    #[test]
    fn the_data_object_offers_the_descriptor_and_a_stream_that_waits_for_the_transfer() {
        let (out, data, dir) = staged("ok", b"12345");
        // The descriptor: one item with the name, the size and the progress flag.
        let f = format(clipboard_format(w!("FileGroupDescriptorW")), TYMED_HGLOBAL.0, -1);
        // SAFETY: a valid FORMATETC; the global block is locked, read and freed.
        unsafe {
            let m = data.GetData(&f).expect("the descriptor");
            let p = GlobalLock(m.u.hGlobal).cast::<u8>();
            assert_eq!(u32::from_le_bytes(std::ptr::read_unaligned(p.cast())), 1);
            let d: FILEDESCRIPTORW = std::ptr::read_unaligned(p.add(4).cast());
            assert_eq!({ d.nFileSizeLow }, 5);
            assert_ne!({ d.dwFlags } & FD_PROGRESSUI, 0);
            let name = { d.cFileName };
            assert_eq!(String::from_utf16_lossy(&name[..5]), "a.bin");
            let _ = GlobalUnlock(m.u.hGlobal);
        }
        assert!(f.cfFormat != 0);

        // A read waits (here, on the "UI thread" of the test, pumping) until the transfer ends, then has the bytes.
        let stream = contents(&data, 0);
        let settle = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            settle.settle_for_test(0, true);
        });
        let started = std::time::Instant::now();
        let (hr, bytes) = read(&stream, 16);
        assert!(started.elapsed() >= Duration::from_millis(250), "the read waited");
        assert_eq!((hr, bytes.as_slice()), (S_OK, &b"12345"[..]));
        assert_eq!(read(&stream, 16).0, S_FALSE, "the end");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_transfer_fails_the_read() {
        let (out, data, dir) = staged("fail", b"12345");
        let stream = contents(&data, 0);
        let settle = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            settle.settle_for_test(0, false);
        });
        let (hr, bytes) = read(&stream, 16);
        assert_eq!((hr, bytes.len()), (STG_E_READFAULT, 0));
        let _ = std::fs::remove_dir_all(dir);
    }
}
