//! Small Windows facts: the app theme, the waker, window metrics, the private GDI font for native inputs.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use swapdesk_ui::UiWaker;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{AddFontMemResourceEx, CreateFontW, FONT_QUALITY, FW_NORMAL, HFONT};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{HSTRING, w};

/// The message the backend's waker posts: drain the events.
pub const WM_WAKE: u32 = WM_APP;
/// A screen reader asked for an action (UI Automation `Invoke`, focus).
pub const WM_ACCESS: u32 = WM_APP + 1;
/// Ends the run (an update about to be installed, a test that is done): the main window is destroyed as a close
/// without the notification area does. A person's close (`WM_CLOSE`) only hides it while the icon is up.
pub const WM_QUIT_APP: u32 = WM_APP + 42;
/// A native text input says something (Enter, Tab, Escape, focus lost): wParam is the input's id, lParam the event.
pub const WM_EDIT_EVENT: u32 = WM_APP + 2;

/// One line to the debugger output: the shell's trace channel (R-UI-10). Names and durations only, never content.
pub fn trace_line(line: &str) {
    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the buffer is NUL-terminated and outlives the call.
    unsafe { windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(windows::core::PCWSTR(wide.as_ptr())) };
}

/// Wakes the UI thread from any thread (`PostMessageW(WM_APP)`, C6). Coalesces: one wake is queued at a time.
pub struct Waker {
    hwnd: AtomicIsize,
    queued: AtomicBool,
}

impl Waker {
    pub fn new() -> Waker {
        Waker { hwnd: AtomicIsize::new(0), queued: AtomicBool::new(false) }
    }

    pub fn set_window(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
    }

    /// The window the waker posts to (null before the main window exists).
    #[cfg(test)]
    pub(crate) fn window(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::SeqCst) as *mut c_void)
    }

    /// The UI thread is about to drain: a new wake may be queued again.
    pub fn drained(&self) {
        self.queued.store(false, Ordering::SeqCst);
    }
}

impl Default for Waker {
    fn default() -> Self {
        Waker::new()
    }
}

impl UiWaker for Waker {
    fn wake(&self) {
        let h = self.hwnd.load(Ordering::SeqCst);
        if h != 0 && !self.queued.swap(true, Ordering::SeqCst) {
            // SAFETY: posting to a window handle from any thread is allowed; a stale handle just fails.
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut c_void)), WM_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// The wall clock and the local UTC offset for `UiState` (dates of the device list, Atividade, chat). Read on every
/// drain of the backend, so a change of time zone or daylight saving shows at the next event.
pub fn local_clock() -> swapdesk_ui::format::Clock {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let mut tz = TIME_ZONE_INFORMATION::default();
    // SAFETY: a plain call with an out pointer to a local.
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    // 2 = TIME_ZONE_ID_DAYLIGHT; UTC = local time + bias (minutes).
    let bias = tz.Bias + if id == 2 { tz.DaylightBias } else { tz.StandardBias };
    swapdesk_ui::format::Clock { now_ms, utc_offset_min: -bias }
}

/// The interface language at start: Brazilian Portuguese when the Windows display language is Portuguese, English
/// otherwise; `SWAPDESK_LANG=pt|en` overrides it in development.
pub fn system_language() -> swapdesk_ui::Language {
    use swapdesk_ui::Language;
    match std::env::var("SWAPDESK_LANG").as_deref() {
        Ok("en") => return Language::En,
        Ok("pt") => return Language::PtBr,
        _ => {}
    }
    // SAFETY: a plain call without arguments.
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    // The primary language is the low 10 bits; LANG_PORTUGUESE = 0x16.
    if id & 0x3FF == 0x16 { Language::PtBr } else { Language::En }
}

/// Whether a contrast theme is on (`SPI_GETHIGHCONTRAST`, `HCF_HIGHCONTRASTON`) and, if so, its colours
/// (`GetSysColor`), learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getsyscolor.
pub fn system_contrast() -> swapdesk_ui::tokens::Contrast {
    use windows::Win32::Graphics::Gdi::{
        COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT,
        COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor, SYS_COLOR_INDEX,
    };
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut hc = HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    // SAFETY: `hc` outlives the call and its size is in `cbSize` and in the second argument.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some(std::ptr::from_mut(&mut hc).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if read.is_err() || !hc.dwFlags.contains(HCF_HIGHCONTRASTON) {
        return swapdesk_ui::tokens::Contrast::Standard;
    }
    // A COLORREF is 0x00BBGGRR; the palette takes 0xRRGGBB.
    let rgb = |index: SYS_COLOR_INDEX| {
        // SAFETY: a plain query.
        let c = unsafe { GetSysColor(index) };
        ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
    };
    swapdesk_ui::tokens::Contrast::Theme(swapdesk_ui::tokens::SystemColors {
        window: rgb(COLOR_WINDOW),
        window_text: rgb(COLOR_WINDOWTEXT),
        hotlight: rgb(COLOR_HOTLIGHT),
        gray_text: rgb(COLOR_GRAYTEXT),
        highlight: rgb(COLOR_HIGHLIGHT),
        highlight_text: rgb(COLOR_HIGHLIGHTTEXT),
        button_face: rgb(COLOR_BTNFACE),
        button_text: rgb(COLOR_BTNTEXT),
    })
}

/// `AppsUseLightTheme` under HKCU (0 means dark); `SWAPDESK_THEME=dark|light` overrides it in development.
pub fn system_dark() -> bool {
    match std::env::var("SWAPDESK_THEME").as_deref() {
        Ok("dark") => return true,
        Ok("light") => return false,
        _ => {}
    }
    let mut data: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `data` and `size` outlive the call and `size` is the byte size of `data`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast::<c_void>()),
            Some(&mut size),
        )
    };
    status.is_ok() && data == 0
}

/// A GUI process has no console: shows a startup failure or the usage text in a message box.
pub fn message_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    // SAFETY: both strings outlive the call.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("Swapdesk"), MB_OK | MB_ICONINFORMATION);
    }
}

/// `%LOCALAPPDATA%\Swapdesk\logs`: where the UI keeps its trace and its crash marker (the same folder as
/// `swapdesk_sys_win::user_log_dir`; the app crate tests that the two agree).
pub fn logs_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("Swapdesk").join("logs"))
}

/// Opens the logs folder with the shell ("open" on the directory). False when there is no folder to open.
pub fn open_logs_folder() -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Some(dir) = logs_dir() else { return false };
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: the strings outlive the call; no window is the parent; the verb opens a directory in Explorer.
    let result = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(dir.as_os_str()), None, None, SW_SHOWNORMAL) };
    // ShellExecuteW succeeds with a value above 32.
    result.0 as isize > 32
}

/// Opens an `https://` page in the default browser (the release page of an update). Anything else is refused.
pub fn open_web_page(url: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if !swapdesk_ui::state::openable_page(url) {
        return false;
    }
    // SAFETY: the strings outlive the call; no window is the parent.
    let result = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL) };
    result.0 as isize > 32
}

/// Opens a file of this computer with the app the system has for it (the explorer's left pane). Only an existing
/// regular file: a folder opens in the explorer itself, and nothing else is ever started.
pub fn open_local_file(path: &std::path::Path) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if !path.is_file() {
        return false;
    }
    // SAFETY: the strings outlive the call; no window is the parent; "open" is the file's default verb.
    let result =
        unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), None, None, SW_SHOWNORMAL) };
    result.0 as isize > 32
}

/// Shows an item of this computer selected in File Explorer (`explorer.exe /select,`). A folder opens itself.
pub fn reveal_local_item(path: &std::path::Path) -> bool {
    let arg = if path.is_dir() {
        std::ffi::OsString::from(path.as_os_str())
    } else {
        let mut a = std::ffi::OsString::from("/select,");
        a.push(path.as_os_str());
        a
    };
    // No shell string is built: the path is one argument of the program.
    std::process::Command::new("explorer.exe").arg(arg).spawn().is_ok()
}

pub fn loword(v: isize) -> u16 {
    (v & 0xFFFF) as u16
}

pub fn hiword(v: isize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

/// Signed x of an `lParam` (mouse messages on a multi-monitor desktop can be negative).
pub fn get_x(lparam: isize) -> i32 {
    i32::from(loword(lparam) as i16)
}

pub fn get_y(lparam: isize) -> i32 {
    i32::from(hiword(lparam) as i16)
}

/// Registers Instrument Sans for this process only, so the native text inputs (GDI) can use it too.
pub fn register_gdi_font() {
    let bytes = crate::gfx::INSTRUMENT_SANS;
    let count = 0u32;
    // SAFETY: the font bytes are `'static`; the registration lives as long as the process.
    unsafe {
        AddFontMemResourceEx(bytes.as_ptr().cast(), bytes.len() as u32, None, &count);
    }
}

/// A GDI font for the native inputs: 14 px at `dpi`, in Instrument Sans (falls back to the system's default face).
pub fn edit_font(dpi: u32) -> HFONT {
    let height = -((14 * dpi as i32 + 48) / 96);
    // SAFETY: plain font creation; the caller deletes it.
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
            windows::Win32::Graphics::Gdi::OUT_DEFAULT_PRECIS,
            windows::Win32::Graphics::Gdi::CLIP_DEFAULT_PRECIS,
            FONT_QUALITY(5),
            0,
            &HSTRING::from(swapdesk_ui::tokens::FONT_FAMILY),
        )
    }
}
