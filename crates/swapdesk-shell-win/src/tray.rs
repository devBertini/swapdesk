//! The notification area icon of a real run (B16): the same actions as the Mac's menu bar item ("Abrir o Swapdesk",
//! "Sair do Swapdesk"). While it is there, closing the main window hides it instead of ending the interface, as the Mac
//! keeps its app in the menu bar (D32): the interface stays connected to the service, so a request that needs a person
//! at this computer still finds one.
//!
//! The icon is drawn here (a 32 px bitmap, the design's blue with two arrows), so the executable needs no icon
//! resource. Win32: `Shell_NotifyIconW` (NIM_ADD, NIM_DELETE) with a callback message to the main window,
//! `CreatePopupMenu` and `TrackPopupMenu` with `TPM_RETURNCMD`, `CreateIconIndirect` over a 32-bit DIB section.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, HICON, ICONINFO,
    MF_STRING, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP,
};
use windows::core::HSTRING;

/// The callback message of the icon, sent to the main window.
pub const WM_TRAY: u32 = WM_APP + 41;
const ICON_ID: u32 = 1;
const SIZE: i32 = 32;

/// What the menu's entry asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Quit,
}

/// The `WM_COMMAND` ids of the menu's two entries (far above the edit controls' small codes).
pub const CMD_OPEN: usize = 0x7101;
pub const CMD_QUIT: usize = 0x7102;

impl TrayAction {
    /// The action a `WM_COMMAND` id of the tray menu stands for.
    pub fn from_command(id: usize) -> Option<TrayAction> {
        match id {
            CMD_OPEN => Some(TrayAction::Open),
            CMD_QUIT => Some(TrayAction::Quit),
            _ => None,
        }
    }
}

/// The icon's pixels, BGRA top-down: a rounded blue square with two arrows that turn into each other (the swap).
pub fn icon_pixels() -> Vec<u8> {
    let n = SIZE as usize;
    let mut px = vec![0u8; n * n * 4];
    let (blue, white) = ([0xE6u8, 0x4A, 0x2E, 0xFF], [0xFFu8, 0xFF, 0xFF, 0xFF]);
    let r = 7.0f32;
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let cx = fx.clamp(r, n as f32 - r);
            let cy = fy.clamp(r, n as f32 - r);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            if !inside {
                continue;
            }
            let mut c = blue;
            // Upper arrow pointing right, lower arrow pointing left.
            let upper = (10..=12).contains(&y) && (8..=21).contains(&x)
                || (y as i32 - 11).abs() + (x as i32 - 22).abs() <= 3 && x >= 19;
            let lower = (19..=21).contains(&y) && (10..=23).contains(&x)
                || (y as i32 - 20).abs() + (9 - x as i32).abs() <= 3 && x <= 12;
            if upper || lower {
                c = white;
            }
            px[(y * n + x) * 4..(y * n + x) * 4 + 4].copy_from_slice(&c);
        }
    }
    px
}

fn make_icon() -> Option<HICON> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: -SIZE,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let pixels = icon_pixels();
    let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: a DIB section of SIZE x SIZE 32-bit pixels is made, filled with exactly that many bytes, turned into an
    // icon (which copies both bitmaps) and the bitmaps are deleted afterwards.
    unsafe {
        let color: HBITMAP = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        core::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: core::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// Puts the icon in the notification area; `false` when the shell refused (the window then closes as before).
pub fn add(hwnd: HWND, tooltip: &str) -> bool {
    let Some(icon) = make_icon() else { return false };
    let mut d = data(hwnd);
    d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon;
    let tip: Vec<u16> = tooltip.encode_utf16().take(d.szTip.len() - 1).collect();
    d.szTip[..tip.len()].copy_from_slice(&tip);
    // SAFETY: a fully initialised structure for our own window; the shell copies the icon.
    let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &d) }.as_bool();
    // SAFETY: the shell keeps its own copy of the icon.
    unsafe {
        let _ = DestroyIcon(icon);
    }
    ok
}

/// Takes the icon away (at the end of the run).
pub fn remove(hwnd: HWND) {
    let d = data(hwnd);
    // SAFETY: deletes our own icon; a missing one is a harmless failure.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &d);
    }
}

/// Shows the menu at the pointer. The entry chosen reaches the window as a `WM_COMMAND` with [`CMD_OPEN`] or
/// [`CMD_QUIT`] (nothing arrives when it is dismissed), so a test can post the same ids.
pub fn menu(hwnd: HWND, open: &str, quit: &str) {
    // SAFETY: a popup menu owned here, shown for our own window (made foreground first, as the shell requires for the
    // menu to close when it loses focus), destroyed before returning; the system posts the command to the window.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, &HSTRING::from(open));
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, &HSTRING::from(quit));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_blue_square_with_white_arrows_and_clear_corners() {
        let px = icon_pixels();
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        let at = |x: usize, y: usize| &px[(y * SIZE as usize + x) * 4..(y * SIZE as usize + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "the corner is transparent");
        assert_eq!(at(16, 4), &[0xE6, 0x4A, 0x2E, 0xFF], "blue inside");
        assert_eq!(at(14, 11), &[0xFF; 4], "the upper arrow");
        assert_eq!(at(16, 20), &[0xFF; 4], "the lower arrow");
    }
}
