//! The macOS calls under the viewer's input: the keyboard state, the keyboard kind, the TCC checks, and the
//! `InputGrabber` over a `CGEventTap`. Every `unsafe` of the crate is here.
//!
//! Apple sources (read 2026-11-08):
//! - `CGEvent.tapCreate`: https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)
//!   ("Event taps receive key up and key down events if ... Access for assistive devices is enabled"; the callback is
//!   invoked from the run loop the tap's source is added to; a `NULL` return from the callback drops the event).
//! - `CGPreflightListenEventAccess` / `CGRequestListenEventAccess` (macOS 10.15) in `CoreGraphics/CGEvent.h`, and
//!   `AXIsProcessTrustedWithOptions` ("Prompting occurs asynchronously and does not affect the return value", with a
//!   `NULL` options dictionary it never prompts) in `HIServices/AXUIElement.h`, both in the macOS 27.2 SDK on the MacBook.
//! - `KBGetLayoutType` ("Returns the physical keyboard layout type", `kKeyboardANSI`/`ISO `/`JIS `) in
//!   `HIToolbox/Keyboards.h`, same SDK; its argument is the keyboard type ID that `CGEventSource.keyboard_type` returns.
//! - Run loops: the Core Foundation run loop is thread safe, so `CFRunLoopStop` may be called from another thread
//!   (Threading Programming Guide, "Thread Safety Summary", developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html).
//! - objc2 bindings: https://docs.rs/objc2-core-graphics/0.3.2/ and https://docs.rs/objc2-core-foundation/0.3.2/.
//!
//! What needs a TCC grant: only the tap (Input Monitoring and Accessibility). The `ViewerInput` events come from the
//! video view through AppKit and need none; reading the key state (`CGEventSourceKeyState`) needs none.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMachPort, CFRetained, CFRunLoop, CFString, kCFBooleanTrue, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGPreflightListenEventAccess, CGRequestListenEventAccess,
};
use swapdesk_keys::KbKind;
use swapdesk_platform::{ErrorKind, InputGrabber, OsGrant, PlatformError, Result};
use swapdesk_proto::InputEvent;
use swapdesk_shell_mac::{KeyMsg, Modifiers};

use crate::input::{Access, GrabStatus, KeyboardState, MacInput};
use crate::keymap::carbon_modifier_state;
use crate::logic::{EventText, Layout, Typed};

// ---------------------------------------------------------------- keyboard state

/// `CGEventSourceKeyState` and `CGEventSourceFlagsState` over the combined session state: the keys and modifiers as
/// the user holds them now, which no event queue delays.
pub struct SystemKeyboardState;

fn aggregate_flags(flags: CGEventFlags) -> Modifiers {
    let mut m = Modifiers::default();
    for (flag, bit) in [
        (CGEventFlags::MaskShift, Modifiers::SHIFT),
        (CGEventFlags::MaskControl, Modifiers::CONTROL),
        (CGEventFlags::MaskAlternate, Modifiers::OPTION),
        (CGEventFlags::MaskCommand, Modifiers::COMMAND),
        (CGEventFlags::MaskAlphaShift, Modifiers::CAPS_LOCK),
        (CGEventFlags::MaskSecondaryFn, Modifiers::FUNCTION),
    ] {
        if flags.contains(flag) {
            m = m.union(bit);
        }
    }
    m
}

impl KeyboardState for SystemKeyboardState {
    fn key_down(&self, kvk: u16) -> bool {
        CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, kvk)
    }

    fn flags(&self) -> Modifiers {
        aggregate_flags(CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState))
    }
}

// ---------------------------------------------------------------- keyboard kind

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// `PhysicalKeyboardLayoutType KBGetLayoutType(SInt16 iKeyboardType)` (HIToolbox/Keyboards.h); "not thread safe".
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
}

const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
const KEYBOARD_JIS: u32 = u32::from_be_bytes(*b"JIS ");

/// The physical kind of the keyboard the Mac reports (ANSI when it cannot tell), for the ISO section/grave swap.
/// Call it from one thread at a time (the Carbon call is documented as not thread safe); the app does it once at start
/// and when the keyboard changes.
pub fn detect_keyboard_kind() -> KbKind {
    let Some(source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
        return KbKind::Ansi;
    };
    let id = CGEventSource::keyboard_type(Some(&source));
    // SAFETY: a pure lookup of the keyboard type ID in Carbon's tables; no pointers are involved. The id is a small
    // number that fits an SInt16 (an out-of-range one is clamped to an unknown type).
    let kind = unsafe { KBGetLayoutType(i16::try_from(id).unwrap_or(0)) };
    match kind {
        KEYBOARD_ISO => KbKind::Iso,
        KEYBOARD_JIS => KbKind::Jis,
        _ => KbKind::Ansi,
    }
}

// ---------------------------------------------------------------- keyboard layout text

// Apple sources (the macOS 27.2 SDK on the MacBook, read 2026-11-10):
// - `UCKeyTranslate` and `kUCKeyActionDown` 0 / `kUCKeyActionAutoKey` 2 in `CarbonCore/UnicodeUtilities.h`: the dead-key
//   state is "a pointer to an unsigned 32-bit value, initialized to zero" that the function keeps between calls; with
//   no translate option a dead key returns no characters and sets that state, and the next key returns the composed
//   character.
// - `TISCopyCurrentKeyboardLayoutInputSource`, `TISCreateInputSourceList`, `TISGetInputSourceProperty`,
//   `kTISPropertyUnicodeKeyLayoutData` (a `CFData` of the `uchr` table) and `kTISPropertyInputSourceID` in
//   `HIToolbox/TextInputSources.h`: "TextInputSources API is not thread safe. If you are a UI application, you must
//   call TextInputSources API on the main thread" (the video view's `ViewerInput::key` is called there; a test process
//   is not a UI application and calls it from one thread).
// - `LMGetKbdType` ("Not thread safe") in `HIToolbox/Events.h`: the keyboard type `UCKeyTranslate` takes.

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISCreateInputSourceList(properties: *const c_void, include_all_installed: u8) -> *const c_void;
    fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
    static kTISPropertyUnicodeKeyLayoutData: &'static CFString;
    static kTISPropertyInputSourceID: &'static CFString;
    fn LMGetKbdType() -> u8;
    #[allow(clippy::too_many_arguments)]
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
}

const UC_KEY_ACTION_DOWN: u16 = 0;
const UC_KEY_ACTION_AUTO_KEY: u16 = 2;

/// One retained `TISInputSourceRef`, released on drop.
struct InputSource(NonNull<c_void>);

// SAFETY: a Core Foundation object's retain count is thread safe, and the Text Input Sources calls made through this
// handle happen behind `MacInput`'s mutex, one thread at a time (the main thread in the app).
unsafe impl Send for InputSource {}

impl Drop for InputSource {
    fn drop(&mut self) {
        // SAFETY: the pointer is a CF object this handle owns one retain of.
        unsafe { CFRelease(self.0.as_ptr()) }
    }
}

impl InputSource {
    /// The keyboard layout in use now (the layout an input method types with, when one is selected).
    fn current() -> Option<Self> {
        // SAFETY: a plain "Copy" call: it returns a +1 reference or NULL.
        NonNull::new(unsafe { TISCopyCurrentKeyboardLayoutInputSource() }).map(Self)
    }

    /// An installed keyboard layout by its id (`com.apple.keylayout.Brazilian-Pro`), selected or not.
    fn named(id: &str) -> Option<Self> {
        // SAFETY: the key is the framework's own constant, alive for the process.
        let key: &CFString = unsafe { kTISPropertyInputSourceID };
        let value = CFString::from_str(id);
        let filter: CFRetained<CFDictionary<CFString, CFString>> = CFDictionary::from_slices(&[key], &[&*value]);
        // SAFETY: `filter` is a valid dictionary for the call; the result is a +1 array or NULL.
        let list = unsafe { TISCreateInputSourceList(std::ptr::from_ref(&*filter).cast(), 1) };
        if list.is_null() {
            return None;
        }
        // SAFETY: `list` is a valid CFArray of input sources; the element is retained before the array is released.
        unsafe {
            let found = if CFArrayGetCount(list) > 0 {
                let source = CFArrayGetValueAtIndex(list, 0);
                NonNull::new(CFRetain(source).cast_mut()).map(Self)
            } else {
                None
            };
            CFRelease(list);
            found
        }
    }

    /// `UCKeyTranslate` of one key press with this layout's Unicode table, keeping `dead_state` between calls.
    /// `None` when the source has no Unicode table or the call fails.
    fn translate(&self, kvk: u16, auto_key: bool, flags: Modifiers, dead_state: &mut u32) -> Option<String> {
        // SAFETY: a property read on a live source; the CFData it returns belongs to the source, which `self` keeps
        // alive for the whole function.
        let data = unsafe {
            let key: &CFString = kTISPropertyUnicodeKeyLayoutData;
            TISGetInputSourceProperty(self.0.as_ptr(), std::ptr::from_ref(key).cast())
        };
        if data.is_null() {
            return None;
        }
        // SAFETY: `data` is a live CFData (see above).
        let layout = unsafe { CFDataGetBytePtr(data) };
        if layout.is_null() {
            return None;
        }
        // SAFETY: a lookup of the keyboard type in Carbon's low memory; called from one thread at a time.
        let keyboard_type = u32::from(unsafe { LMGetKbdType() });
        let mut buf = [0u16; 16];
        let mut len = 0usize;
        let action = if auto_key { UC_KEY_ACTION_AUTO_KEY } else { UC_KEY_ACTION_DOWN };
        // SAFETY: `layout` points to the `uchr` table the source owns; the state, length and buffer are valid for the
        // sizes given.
        let status = unsafe {
            UCKeyTranslate(
                layout.cast(),
                kvk,
                action,
                carbon_modifier_state(flags),
                keyboard_type,
                0,
                dead_state,
                buf.len(),
                &mut len,
                buf.as_mut_ptr(),
            )
        };
        if status != 0 {
            *dead_state = 0;
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len.min(buf.len())]))
    }
}

/// What a key press types on this Mac: the keyboard layout's own table through `UCKeyTranslate`, with its dead keys
/// (the state is kept here between presses, so `'` then `a` is `á` on "Brazilian - Pro" and Option+E then E is `é`
/// on a US layout). A source with no Unicode table falls back to the event's characters.
pub struct MacLayout {
    /// `None`: the layout selected at each press.
    fixed: Option<InputSource>,
    dead_state: u32,
}

impl MacLayout {
    /// Follows the layout the user selects.
    pub fn current() -> Self {
        Self { fixed: None, dead_state: 0 }
    }

    /// One installed layout by its input source id, whatever is selected (tests). `None` when it is not installed.
    pub fn named(id: &str) -> Option<Self> {
        InputSource::named(id).map(|s| Self { fixed: Some(s), dead_state: 0 })
    }
}

impl Layout for MacLayout {
    fn type_key(&mut self, msg: &KeyMsg) -> Typed {
        let current;
        let source = match &self.fixed {
            Some(s) => Some(s),
            None => {
                current = InputSource::current();
                current.as_ref()
            }
        };
        let Some(text) =
            source.and_then(|s| s.translate(msg.key_code, msg.repeat, msg.modifiers, &mut self.dead_state))
        else {
            self.dead_state = 0;
            return EventText.type_key(msg);
        };
        if text.is_empty() {
            return Typed { text: None, dead: self.dead_state != 0 };
        }
        Typed { text: Some(text), dead: false }
    }

    fn clear_dead_key(&mut self) {
        self.dead_state = 0;
    }
}

// ---------------------------------------------------------------- TCC

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// `Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef options)`: with `NULL` it only reports.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    /// `kAXTrustedCheckOptionPrompt`: the key of the prompt option.
    static kAXTrustedCheckOptionPrompt: &'static CFString;
}

/// The two grants the system-key grab needs, read without prompting.
pub fn access() -> Access {
    // SAFETY: `NULL` options is the documented way to ask without prompting.
    let accessibility = unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) } != 0;
    Access { input_monitoring: CGPreflightListenEventAccess(), accessibility }
}

/// Asks macOS for what is missing, once per call (the UI's "Permitir" button, never a timer): Input Monitoring through
/// `CGRequestListenEventAccess` and Accessibility through `AXIsProcessTrustedWithOptions` with the prompt option. Each
/// prompt appears only the first time; afterwards the user must switch the app on in System Settings, so the answer is
/// read again with [`access`] and the pane opened by the app. Returns the grants as they are right after the call (the
/// user's answer arrives later).
pub fn request_access() -> Access {
    let have = access();
    if !have.input_monitoring {
        let _ = CGRequestListenEventAccess();
    }
    if !have.accessibility
        && let Some(yes) = unsafe { kCFBooleanTrue }
    {
        // SAFETY: the key is the framework's own constant, alive for the process.
        let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
        let options: CFRetained<CFDictionary<CFString, CFBoolean>> = CFDictionary::from_slices(&[key], &[yes]);
        // SAFETY: `options` is a valid dictionary for the call; the prompt is asynchronous and does not affect the result.
        let _ = unsafe { AXIsProcessTrustedWithOptions(std::ptr::from_ref(&*options).cast()) };
    }
    access()
}

// ---------------------------------------------------------------- the tap

/// What the callback reaches: the input to feed and the tap's own port (to switch it back on when macOS disables it).
struct TapCtx {
    input: MacInput,
    port: AtomicPtr<CFMachPort>,
}

fn key_flags(event: &CGEvent) -> Modifiers {
    aggregate_flags(CGEvent::flags(Some(event)))
}

/// Callback of the tap, on the tap thread's run loop. It must answer fast (macOS disables a tap that does not) so it
/// does a lock, table lookups and a `try_send`, and nothing else. Returning null drops the event.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `info` is the `TapCtx` the thread made; the thread keeps it alive until the tap is gone.
    let ctx = unsafe { &*info.cast::<TapCtx>() };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        let port = ctx.port.load(Ordering::Acquire);
        if !port.is_null() {
            // SAFETY: the port is alive while the thread runs; it clears the pointer before dropping it.
            CGEvent::tap_enable(unsafe { &*port }, true);
        }
        return event.as_ptr();
    }
    if kind == CGEventType::KeyDown || kind == CGEventType::KeyUp {
        // SAFETY: the event pointer is valid for the duration of the callback.
        let ev = unsafe { event.as_ref() };
        let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode);
        if let Ok(kvk) = u16::try_from(code)
            && ctx.input.tap_key(kvk, kind == CGEventType::KeyDown, key_flags(ev))
        {
            return std::ptr::null_mut();
        }
    }
    event.as_ptr()
}

/// A `CFRunLoop` another thread may stop.
struct RunLoopHandle(CFRetained<CFRunLoop>);

// SAFETY: a Core Foundation run loop is documented as thread safe (Threading Programming Guide, Thread Safety Summary,
// address in the module comment); the only use made of this handle is `CFRunLoopStop` from the owner's thread.
unsafe impl Send for RunLoopHandle {}

struct TapThread {
    run_loop: RunLoopHandle,
    join: JoinHandle<()>,
}

fn os_failure(op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Unavailable, op, os_code: None, detail: detail.to_owned() }
}

fn run_tap(ctx: &TapCtx, ready: &SyncSender<Result<RunLoopHandle>>) {
    let mask: u64 = (1 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0);
    // SAFETY: the callback is a valid function and `ctx` outlives the tap (this function returns only after the tap is
    // disabled and invalidated).
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            std::ptr::from_ref(ctx).cast_mut().cast(),
        )
    };
    let Some(tap) = tap else {
        let _ = ready.send(Err(os_failure("CGEventTapCreate", "returned NULL although the grants were present")));
        return;
    };
    let (Some(source), Some(run_loop)) = (CFMachPort::new_run_loop_source(None, Some(&tap), 0), CFRunLoop::current())
    else {
        let _ = ready.send(Err(os_failure("CFMachPortCreateRunLoopSource", "no run loop source")));
        return;
    };
    // SAFETY: the mode constant is a valid CFString for the process.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    ctx.port.store(std::ptr::from_ref::<CFMachPort>(&tap).cast_mut(), Ordering::Release);
    CGEvent::tap_enable(&tap, true);
    if ready.send(Ok(RunLoopHandle(run_loop))).is_err() {
        // The starter gave up waiting: do not leave a tap behind.
        ctx.port.store(std::ptr::null_mut(), Ordering::Release);
        tap.invalidate();
        return;
    }
    CFRunLoop::run();
    ctx.port.store(std::ptr::null_mut(), Ordering::Release);
    CGEvent::tap_enable(&tap, false);
    tap.invalidate();
}

fn spawn_tap(input: &MacInput) -> Result<TapThread> {
    let (sync_tx, sync_rx) = sync_channel::<Result<RunLoopHandle>>(1);
    let ctx = TapCtx { input: input.clone(), port: AtomicPtr::new(std::ptr::null_mut()) };
    let join = std::thread::Builder::new()
        .name("swapdesk-event-tap".to_owned())
        .spawn(move || run_tap(&ctx, &sync_tx))
        .map_err(|e| PlatformError {
            kind: ErrorKind::Os,
            op: "spawn event tap thread",
            os_code: e.raw_os_error().map(i64::from),
            detail: e.to_string(),
        })?;
    match sync_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(run_loop)) => Ok(TapThread { run_loop, join }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => Err(os_failure("event tap", "the tap thread did not report in time")),
    }
}

impl TapThread {
    fn stop(self) {
        self.run_loop.0.stop();
        let _ = self.join.join();
    }
}

/// The `InputGrabber` of the Mac viewer. The keys of the video view come through [`MacInput`] (the shell's
/// `ViewerInput`); this adds what the view never sees: with "capturar atalhos do sistema" on and the viewer focused, an
/// event tap swallows Cmd+Tab, Cmd+Space, the Mission Control and Spaces shortcuts and the like and sends them to the
/// host instead (F-INPUT-04).
///
/// `start` is the shell's "viewer focused" call and `stop` its "focus lost": `stop` releases every key and button on the
/// host. Without the grants the tap is not made and [`MacInput::grab_status`] says which are missing; nothing prompts
/// unless the UI calls [`MacGrabber::request_access`], and nothing retries by itself.
pub struct MacGrabber {
    input: MacInput,
    wanted: bool,
    started: bool,
    tap: Option<TapThread>,
}

fn permission_error(access: Access) -> PlatformError {
    let grant = if access.input_monitoring { OsGrant::Accessibility } else { OsGrant::InputMonitoring };
    PlatformError {
        kind: ErrorKind::PermissionDenied(grant),
        op: "InputGrabber.set_grab_system_keys",
        os_code: None,
        detail: format!(
            "missing: input monitoring {}, accessibility {}",
            !access.input_monitoring, !access.accessibility
        ),
    }
}

impl MacGrabber {
    pub fn new(input: MacInput) -> Self {
        Self { input, wanted: false, started: false, tap: None }
    }

    /// The input this grabber feeds (hand a clone to the shell as its `ViewerInput`).
    pub fn input(&self) -> &MacInput {
        &self.input
    }

    /// The grants as they are now (never prompts).
    pub fn access(&self) -> Access {
        access()
    }

    /// The UI's "Permitir" action: raises the macOS prompts that have not been shown yet. See [`request_access`].
    pub fn request_access(&self) -> Access {
        request_access()
    }

    /// Tries again to make the tap after the user granted access (the UI calls it from a button or when the app comes
    /// back to the front, once per user action).
    pub fn retry(&mut self) -> Result<()> {
        self.set_grab_system_keys(self.wanted)
    }

    fn start_tap(&mut self) -> Result<()> {
        if self.tap.is_some() {
            return Ok(());
        }
        let have = access();
        if !have.complete() {
            self.input.set_grab_status(GrabStatus::NeedsPermission(have));
            return Err(permission_error(have));
        }
        match spawn_tap(&self.input) {
            Ok(tap) => {
                self.tap = Some(tap);
                self.input.set_grab_status(GrabStatus::Active);
                Ok(())
            }
            Err(e) => {
                self.input.set_grab_status(GrabStatus::Failed);
                Err(e)
            }
        }
    }

    fn stop_tap(&mut self) {
        if let Some(tap) = self.tap.take() {
            tap.stop();
        }
        self.input.set_grab_status(GrabStatus::Off);
    }
}

impl InputGrabber for MacGrabber {
    fn start(&mut self, events: SyncSender<InputEvent>) -> Result<()> {
        if self.started {
            return Err(os_failure("InputGrabber.start", "already started"));
        }
        self.started = true;
        self.input.attach(events);
        if self.wanted {
            // A missing grant is reported through the status; the keyboard of the view works without it.
            let _ = self.start_tap();
        }
        Ok(())
    }

    fn set_grab_system_keys(&mut self, on: bool) -> Result<()> {
        self.wanted = on;
        if !self.started {
            return Ok(());
        }
        if on {
            return self.start_tap();
        }
        self.stop_tap();
        // A swallowed key still held: macOS will never see its release, so the host lets go of it.
        self.input.system_keys_off();
        Ok(())
    }

    fn stop(&mut self) {
        if !self.started {
            return;
        }
        self.stop_tap();
        self.input.detach();
        self.started = false;
    }
}

impl Drop for MacGrabber {
    fn drop(&mut self) {
        self.stop();
    }
}
