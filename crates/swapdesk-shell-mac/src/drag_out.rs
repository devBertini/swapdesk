//! Slice 5 (`docs/FILES-UI.md` 4.3): the files of the other computer dragged out of the explorer's right pane into
//! the Finder or onto the desktop, as file promises.
//!
//! One `NSFilePromiseProvider` per file, inside one `NSDraggingItem` each, started on the canvas with
//! `beginDraggingSessionWithItems:event:source:`. Each provider's delegate answers `fileNameForType:` and, after the
//! drop, `writePromiseToURL:completionHandler:` on an operation queue of its own (never the main queue): it asks the
//! state for the download into the staging folder ([`DragOut::request`]), waits for the verified file, moves it to
//! the promised URL (through a temporary name beside it, so the destination never holds a partial file) and calls the
//! completion handler, with an error when the transfer failed.
//!
//! The delegates are plain Objective-C classes without the generated protocol trait: that trait needs `block2` and
//! `NSOperationQueue`, features this crate does not enable (Cargo files are not this slice's). AppKit looks the
//! selectors up at run time, so conformance is not needed; the completion handler block is called through its
//! invoke pointer.

use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, Mutex};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource,
    NSFilePromiseProvider, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, ns_string,
};
use swapdesk_ui::DragOut;

use crate::canvas::CanvasView;

/// Lets the drag out go (cancels what runs, removes the staging folder) when the last holder is gone: the dragging
/// source and every file's delegate each hold one.
struct Guard(Arc<DragOut>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    static kUTTagClassFilenameExtension: *const c_void;
    static kUTTypeData: *const c_void;
    fn UTTypeCreatePreferredIdentifierForTag(
        tag_class: *const c_void,
        tag: *const c_void,
        conforming_to: *const c_void,
    ) -> *mut NSString;
}

/// The UTI of a file name's extension, `public.data` when the system knows none.
fn file_type(name: &str) -> Retained<NSString> {
    let fallback = || NSString::from_str("public.data");
    let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()).filter(|e| !e.is_empty()) else {
        return fallback();
    };
    let ext = NSString::from_str(ext);
    // SAFETY: a CFString is an NSString (toll-free); the call follows the Create rule (the result is owned, or null).
    unsafe {
        let uti = UTTypeCreatePreferredIdentifierForTag(
            kUTTagClassFilenameExtension,
            std::ptr::from_ref::<NSString>(&ext).cast(),
            kUTTypeData,
        );
        Retained::from_raw(uti).unwrap_or_else(fallback)
    }
}

/// The layout of a block: the pointer to call is `invoke`.
#[repr(C)]
struct BlockLayout {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*mut BlockLayout, *mut AnyObject),
}

/// Calls the completion handler (`void (^)(NSError *)`) with no error, or with one.
fn complete(handler: *mut c_void, ok: bool) {
    if handler.is_null() {
        return;
    }
    // SAFETY: a domain string, a code and no user info.
    let error =
        (!ok).then(|| unsafe { NSError::errorWithDomain_code_userInfo(ns_string!("SwapdeskDragOut"), 1, None) });
    let arg = error.as_ref().map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e).cast_mut().cast::<AnyObject>());
    let block = handler.cast::<BlockLayout>();
    // SAFETY: AppKit passes a live block that stays valid during this call; `invoke` takes the block and the error.
    unsafe { ((*block).invoke)(block, arg) };
}

/// Moves the verified `src` to `dest`: a rename when both lie on one volume, else a copy under a temporary name
/// beside `dest` and a rename of that, so `dest` is never a partial file.
fn move_into_place(src: &Path, dest: &Path) -> std::io::Result<()> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dest.with_file_name(format!(".{name}.swapdesk-tmp"));
    let done = std::fs::copy(src, &tmp).and_then(|_| std::fs::rename(&tmp, dest));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return done;
    }
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// Keeps the promise of file `index`: the download, the wait, the move.
fn keep_promise(out: &Arc<DragOut>, index: usize, dest: &Path) -> bool {
    if out.request(index).is_err() {
        return false;
    }
    match out.wait_done(index) {
        Ok(src) => move_into_place(&src, dest).is_ok(),
        Err(_) => false,
    }
}

struct DelegateIvars {
    out: Arc<DragOut>,
    index: usize,
    name: String,
    queue: Retained<AnyObject>,
    /// Taken (dropped) when the promise is kept or broken.
    guard: Mutex<Option<Arc<Guard>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no `Drop`. It is used from AppKit's operation
    // queue, so it is not main-thread-only; its ivars are thread-safe (`Arc`, `Mutex`, an immutable queue).
    #[unsafe(super = NSObject)]
    #[name = "SwapdeskPromiseDelegate"]
    #[ivars = DelegateIvars]
    struct PromiseDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for PromiseDelegate {}

    impl PromiseDelegate {
        // SAFETY (all methods): the selectors and signatures are those of `NSFilePromiseProviderDelegate`.
        #[unsafe(method(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _provider: &AnyObject, _file_type: &AnyObject) -> *mut NSString {
            Retained::autorelease_return(NSString::from_str(&self.ivars().name))
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(&self, _provider: &AnyObject, url: &NSURL, handler: *mut c_void) {
            let iv = self.ivars();
            let dest = url.path().map(|p| std::path::PathBuf::from(p.to_string()));
            let ok = dest.is_some_and(|d| keep_promise(&iv.out, iv.index, &d));
            complete(handler, ok);
            iv.guard.lock().unwrap_or_else(|e| e.into_inner()).take();
        }

        #[unsafe(method(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &AnyObject) -> *mut AnyObject {
            Retained::as_ptr(&self.ivars().queue).cast_mut()
        }
    }
);

/// A delegate kept alive for the promises still to be written (the provider holds its delegate weakly).
struct Keep(#[allow(dead_code)] Retained<PromiseDelegate>);

// SAFETY: the delegate's state is thread-safe (see the class) and it is only kept, never used, through this handle.
unsafe impl Send for Keep {}

static LIVE: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

struct SourceIvars {
    guard: std::cell::RefCell<Option<Arc<Guard>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskDragOutSource"]
    #[ivars = SourceIvars]
    struct DragSource;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for DragSource {}

    // SAFETY: the signatures are `NSDraggingSource`'s.
    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_mask(&self, _session: &NSDraggingSession, context: NSDraggingContext) -> NSDragOperation {
            // A copy outside the app; inside it the painted drag of the explorer is the only one.
            if context == NSDraggingContext::OutsideApplication { NSDragOperation::Copy } else { NSDragOperation::None }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _at: NSPoint, operation: NSDragOperation) {
            self.ivars().guard.borrow_mut().take();
            if operation == NSDragOperation::None {
                // Nobody took the promises: the delegates go, and with the last guard the staging folder.
                LIVE.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
        }
    }
);

thread_local! {
    static ACTIVE: std::cell::RefCell<Option<Retained<DragSource>>> = const { std::cell::RefCell::new(None) };
}

/// Starts the dragging session of `out` on `canvas`, with the mouse event being handled. Returns false when it
/// could not start (the drag out is released).
pub(crate) fn begin(canvas: &CanvasView, out: Arc<DragOut>, mtm: MainThreadMarker) -> bool {
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        out.release();
        return false;
    };
    out.set_waker(Arc::new(|| {
        DispatchQueue::main().exec_async(crate::shell::system_drag_pump);
    }));
    // What an earlier drag left (promises never asked for) goes now.
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    ACTIVE.with(|a| a.borrow_mut().take());

    let guard = Arc::new(Guard(out.clone()));
    // SAFETY: `new` on a Foundation class; the queue is used only by AppKit and the delegates.
    let queue: Retained<AnyObject> = unsafe { msg_send![class!(NSOperationQueue), new] };
    // SAFETY: a plain setter on the queue just made.
    let _: () = unsafe { msg_send![&*queue, setMaxConcurrentOperationCount: 2isize] };
    let workspace = NSWorkspace::sharedWorkspace();
    let at = canvas.convertPoint_fromView(event.locationInWindow(), None);
    let mut items = Vec::new();
    for (i, f) in out.files().iter().enumerate() {
        let this = PromiseDelegate::alloc().set_ivars(DelegateIvars {
            out: out.clone(),
            index: i,
            name: f.name.clone(),
            queue: queue.clone(),
            guard: Mutex::new(Some(guard.clone())),
        });
        // SAFETY: `init` is NSObject's initializer.
        let delegate: Retained<PromiseDelegate> = unsafe { msg_send![super(this), init] };
        let uti = file_type(&f.name);
        // SAFETY: `initWithFileType:delegate:` takes a UTI string and the delegate object.
        let provider: Retained<NSFilePromiseProvider> =
            unsafe { msg_send![NSFilePromiseProvider::alloc(), initWithFileType: &*uti, delegate: &*delegate] };
        let item =
            NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*provider));
        // `iconForContentType:` needs `UTType` (UniformTypeIdentifiers, not linked here); this one takes the UTI.
        #[allow(deprecated)]
        let icon = workspace.iconForFileType(&uti);
        let shift = 6.0 * i.min(5) as f64;
        let frame = NSRect::new(NSPoint::new(at.x - 16.0 + shift, at.y - 16.0 + shift), NSSize::new(32.0, 32.0));
        // SAFETY: the contents are an NSImage.
        unsafe { item.setDraggingFrame_contents(frame, Some(&icon)) };
        LIVE.lock().unwrap_or_else(|e| e.into_inner()).push(Keep(delegate));
        items.push(item);
    }
    let source = DragSource::alloc(mtm).set_ivars(SourceIvars { guard: std::cell::RefCell::new(Some(guard)) });
    // SAFETY: `init` is NSObject's initializer.
    let source: Retained<DragSource> = unsafe { msg_send![super(source), init] };
    let array = NSArray::from_retained_slice(&items);
    let _session =
        canvas.beginDraggingSessionWithItems_event_source(&array, &event, ProtocolObject::from_ref(&*source));
    ACTIVE.with(|a| *a.borrow_mut() = Some(source));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-mac-dragout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_verified_file_moves_to_the_promised_url_and_leaves_nothing_behind() {
        let d = temp("move");
        std::fs::write(d.join("src.bin"), b"12345").unwrap();
        move_into_place(&d.join("src.bin"), &d.join("dest.bin")).unwrap();
        assert_eq!(std::fs::read(d.join("dest.bin")).unwrap(), b"12345");
        assert!(!d.join("src.bin").exists());
        // A missing source leaves no destination and no temporary file.
        assert!(move_into_place(&d.join("none.bin"), &d.join("x.bin")).is_err());
        assert!(!d.join("x.bin").exists() && !d.join(".x.bin.swapdesk-tmp").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_file_type_comes_from_the_extension_with_a_data_fallback() {
        assert_eq!(file_type("a.txt").to_string(), "public.plain-text");
        assert_eq!(file_type("a.jpg").to_string(), "public.jpeg");
        assert_eq!(file_type("noext").to_string(), "public.data");
    }

    #[test]
    fn a_promise_of_a_failed_transfer_is_broken_and_leaves_no_file() {
        let d = temp("promise");
        let files = vec![swapdesk_ui::DragOutFile { name: "a.bin".into(), size: 5, mtime_ms: 0 }];
        let out = DragOut::new(1, String::new(), files, d.join("stage"));
        let settle = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            settle.settle_for_test(0, false);
        });
        assert!(!keep_promise(&out, 0, &d.join("dest.bin")));
        assert!(!d.join("dest.bin").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_promise_of_a_finished_transfer_lands_whole_in_the_destination() {
        let d = temp("kept");
        let files = vec![swapdesk_ui::DragOutFile { name: "a.bin".into(), size: 5, mtime_ms: 0 }];
        let out = DragOut::new(1, String::new(), files, d.join("stage"));
        let (settle, stage) = (out.clone(), d.join("stage"));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::fs::create_dir_all(&stage).unwrap();
            std::fs::write(stage.join("a.bin"), b"12345").unwrap();
            settle.settle_for_test(0, true);
        });
        assert!(keep_promise(&out, 0, &d.join("dest.bin")));
        assert_eq!(std::fs::read(d.join("dest.bin")).unwrap(), b"12345");
        let _ = std::fs::remove_dir_all(d);
    }
}
