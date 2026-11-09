//! Full screen for the viewer window (D30, F11): the window loses its frame and covers one monitor; leaving puts the
//! style and the placement back. Which monitor is the state's choice (`UiState::fullscreen_monitor`, an index of
//! [`monitor_rects`]), or the one the window is on.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, GetWindowPlacement, HWND_TOP, IsZoomed, SW_RESTORE, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow,
    WINDOWPLACEMENT, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

/// What leaving full screen puts back.
pub struct Saved {
    style: isize,
    placement: WINDOWPLACEMENT,
}

unsafe extern "system" fn collect(monitor: HMONITOR, _dc: HDC, _rect: *mut RECT, data: LPARAM) -> windows::core::BOOL {
    // SAFETY: `data` is the `Vec` that `monitor_handles` passes and keeps alive for the whole enumeration.
    let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    list.push(monitor);
    true.into()
}

fn rect_of(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: a plain query into a local.
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool().then_some(info.rcMonitor)
}

/// The monitors' rectangles in virtual-screen pixels, left to right then top to bottom: the order of the choice list.
pub fn monitor_rects() -> Vec<RECT> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into the `Vec`, which outlives the call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(std::ptr::from_mut(&mut handles) as isize));
    }
    let mut rects: Vec<RECT> = handles.into_iter().filter_map(rect_of).collect();
    rects.sort_by_key(|r| (r.left, r.top));
    rects
}

/// The rectangle full screen covers: the chosen monitor, else the one the window is on.
pub fn target_rect(hwnd: HWND, monitor: Option<u8>) -> Option<RECT> {
    monitor
        .and_then(|i| monitor_rects().get(usize::from(i)).copied())
        // SAFETY: a plain query about our own window.
        .or_else(|| rect_of(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }))
}

/// Takes the frame off and covers `target`.
pub fn enter(hwnd: HWND, target: RECT) -> Saved {
    let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    // SAFETY: plain Win32 calls on our own window.
    unsafe {
        let _ = GetWindowPlacement(hwnd, &mut placement);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if IsZoomed(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let framed = (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style & !framed);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
        Saved { style, placement }
    }
}

/// Puts the frame and the placement back.
pub fn leave(hwnd: HWND, saved: &Saved) {
    // SAFETY: plain Win32 calls on our own window.
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, saved.style);
        let _ = SetWindowPlacement(hwnd, &saved.placement);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monitor_list_is_ordered_left_to_right_and_never_empty_on_a_desktop_session() {
        let rects = monitor_rects();
        assert!(!rects.is_empty(), "a session with a desktop has a monitor");
        assert!(rects.windows(2).all(|w| (w[0].left, w[0].top) <= (w[1].left, w[1].top)));
        assert!(rects.iter().all(|r| r.right > r.left && r.bottom > r.top));
    }
}
