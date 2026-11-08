//! Core Graphics checks: `cgstream` (deprecated capture fallback, P5), `inject` (CGEventPost, P4.6 and P6),
//! `keytap` (CGEventTap, P9), and the TCC preflight/request calls (P4, P7).

use crate::d;
use crate::log::emit;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2_core_foundation::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFMachPort, CFRunLoop};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayPixelsHigh, CGDisplayPixelsWide, CGDisplayStream, CGDisplayStreamFrameStatus,
    CGDisplayStreamUpdate, CGError, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMainDisplayID,
    CGMouseButton, CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess,
    CGRequestListenEventAccess, CGRequestPostEventAccess, CGRequestScreenCaptureAccess,
};
use objc2_io_surface::IOSurfaceRef;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `kCVPixelFormatType_32BGRA` as the `i32` CGDisplayStream takes.
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;

pub fn tcc(request: bool) {
    let before = (
        CGPreflightScreenCaptureAccess(),
        CGPreflightPostEventAccess(),
        CGPreflightListenEventAccess(),
        ax_trusted(false),
    );
    if request {
        // Each request call may raise its own prompt; the answer is read again with preflight below.
        let _ = CGRequestScreenCaptureAccess();
        let _ = CGRequestPostEventAccess();
        let _ = CGRequestListenEventAccess();
        let _ = ax_trusted(true);
        std::thread::sleep(Duration::from_secs(2));
    }
    let after = (
        CGPreflightScreenCaptureAccess(),
        CGPreflightPostEventAccess(),
        CGPreflightListenEventAccess(),
        ax_trusted(false),
    );
    emit(
        "tcc",
        after.0 && after.1 && after.3,
        &d!(
            "screen_capture" => after.0,
            "post_event" => after.1,
            "listen_event" => after.2,
            "ax_trusted" => after.3,
            "requested" => request,
            "before_screen_capture" => before.0,
            "before_post_event" => before.1,
            "before_listen_event" => before.2,
            "before_ax_trusted" => before.3
        ),
    );
}

/// `AXIsProcessTrusted`. With `prompt` this probe relies on `CGRequestPostEventAccess`, which raises the
/// Accessibility pane entry; the prompting variant `AXIsProcessTrustedWithOptions` is not used (GUESS that
/// the request call is enough, P4.4 records whether the prompt appeared).
fn ax_trusted(_prompt: bool) -> bool {
    // SAFETY: AXIsProcessTrusted has no preconditions.
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

#[allow(deprecated)]
pub fn cgstream(seconds: u64) {
    let display = CGMainDisplayID();
    let (w, h) = (CGDisplayPixelsWide(display), CGDisplayPixelsHigh(display));
    let frames = Arc::new(AtomicU64::new(0));
    let counter = frames.clone();
    let block = RcBlock::new(
        move |status: CGDisplayStreamFrameStatus, _time: u64, surface: *mut IOSurfaceRef, _update: *const CGDisplayStreamUpdate| {
            if status == CGDisplayStreamFrameStatus::FrameComplete && !surface.is_null() {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    let queue = DispatchQueue::new("local.swapdesk.probe.cgstream", None);
    // SAFETY: the queue and the block outlive the stream, which is stopped before they drop.
    let stream = unsafe {
        CGDisplayStream::with_dispatch_queue(
            display,
            w,
            h,
            PIXEL_FORMAT_BGRA,
            None,
            &queue,
            RcBlock::as_ptr(&block),
        )
    };
    let Some(stream) = stream else {
        emit("cgstream.create", false, &d!("error" => "CGDisplayStreamCreateWithDispatchQueue returned NULL"));
        return;
    };
    let started = CGDisplayStream::start(Some(&stream));
    if started != CGError::Success {
        emit("cgstream.start", false, &d!("cg_error" => i64::from(started.0)));
        return;
    }
    std::thread::sleep(Duration::from_secs(seconds));
    let _ = CGDisplayStream::stop(Some(&stream));
    let n = frames.load(Ordering::Relaxed);
    emit("cgstream.frames", n > 0, &d!("frames" => n, "width" => w, "height" => h));
}

static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// kVK_Shift
const KVK_SHIFT: u16 = 0x38;

/// After `delay` seconds: a mouse square at the screen centre, Shift down/up, then `text` typed through
/// `CGEventKeyboardSetUnicodeString`, no Return. The outcome is visual, so the line records what was posted.
pub fn inject(text: &str, delay: u64, keycode: u16) {
    emit("inject.begin", true, &d!("delay_s" => delay, "post_event_preflight" => CGPreflightPostEventAccess()));
    std::thread::sleep(Duration::from_secs(delay));
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let bounds = CGDisplayBounds(CGMainDisplayID());
    let (cx, cy) = (bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
    let corners = [(-100.0, -100.0), (100.0, -100.0), (100.0, 100.0), (-100.0, 100.0)];
    let mut posted = 0u32;
    for (dx, dy) in corners {
        let point = objc2_core_foundation::CGPoint { x: cx + dx, y: cy + dy };
        if let Some(ev) = CGEvent::new_mouse_event(source.as_deref(), CGEventType::MouseMoved, point, CGMouseButton::Left) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            posted += 1;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    for down in [true, false] {
        if let Some(ev) = CGEvent::new_keyboard_event(source.as_deref(), KVK_SHIFT, down) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            posted += 1;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for ch in text.chars() {
        let mut units = [0u16; 2];
        let encoded = ch.encode_utf16(&mut units);
        for down in [true, false] {
            if let Some(ev) = CGEvent::new_keyboard_event(source.as_deref(), keycode, down) {
                // SAFETY: `encoded` is a valid UTF-16 buffer of the given length.
                unsafe { CGEvent::keyboard_set_unicode_string(Some(&ev), encoded.len() as u64, encoded.as_ptr()) };
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
                posted += 1;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    emit(
        "inject.posted",
        posted > 0,
        &d!("events_posted" => posted, "text" => text, "keycode" => u64::from(keycode), "visual_check" => "pointer square at screen centre; text in the focused field"),
    );
}

/// Callback of the tap: logs each key and, when `active`, swallows Cmd+Tab.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _info: *mut c_void,
) -> *mut CGEvent {
    let active = ACTIVE.load(Ordering::Relaxed);
    // SAFETY: the event pointer is valid for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    if kind == CGEventType::KeyDown || kind == CGEventType::KeyUp || kind == CGEventType::FlagsChanged {
        let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode);
        let flags = CGEvent::flags(Some(ev));
        let mut buf = [0u16; 4];
        let mut len = 0u64;
        // SAFETY: buffer and length pointers are valid for 4 UTF-16 units.
        unsafe { CGEvent::keyboard_get_unicode_string(Some(ev), 4, &mut len, buf.as_mut_ptr()) };
        let character = String::from_utf16_lossy(&buf[..(len as usize).min(4)]);
        let swallow = active && kind != CGEventType::FlagsChanged && code == 48 && flags.contains(CGEventFlags::MaskCommand);
        emit(
            "keytap.key",
            true,
            &d!(
                "kind" => i64::from(kind.0),
                "kvk" => code,
                "flags_hex" => format!("{:#x}", flags.0),
                "character" => character,
                "keyboard_type" => CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeyboardType),
                "swallowed" => swallow
            ),
        );
        if swallow {
            return std::ptr::null_mut();
        }
    }
    event.as_ptr()
}

pub fn keytap(seconds: u64, active: bool) {
    let mask: u64 = (1 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0) | (1 << CGEventType::FlagsChanged.0);
    let options = if active { CGEventTapOptions::Default } else { CGEventTapOptions::ListenOnly };
    ACTIVE.store(active, Ordering::Relaxed);
    // SAFETY: the callback is a valid extern fn; `info` is only tested for null.
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            options,
            mask,
            Some(tap_callback),
            std::ptr::null_mut(),
        )
    };
    let Some(tap) = tap else {
        emit(
            "keytap.create",
            false,
            &d!("active" => active, "error" => "CGEventTapCreate returned NULL: Input Monitoring (listen) or Accessibility (active) missing", "listen_event" => CGPreflightListenEventAccess(), "ax_trusted" => ax_trusted(false)),
        );
        return;
    };
    emit("keytap.create", true, &d!("active" => active, "seconds" => seconds));
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
        emit("keytap.source", false, &d!("error" => "no run loop source"));
        return;
    };
    if let Some(rl) = CFRunLoop::current() {
        // SAFETY: the mode constant is a valid CFString.
        rl.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    }
    CGEvent::tap_enable(&tap, true);
    // SAFETY: the mode constant is a valid CFString.
    unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, seconds as f64, false) };
    emit("keytap.done", true, &d!("seconds" => seconds));
}
