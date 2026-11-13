//! Files dragged from the Finder into the session window (FILES-UI.md 4.2, slice 4): the `NSDraggingDestination`
//! side of the canvas view and of the video view below it.
//!
//! Apple: registering an `NSView` for dragged types makes it a candidate destination; the window offers a drag to the
//! view under the pointer (the video view where the canvas lets the mouse through, the canvas elsewhere), so both
//! register and both call these functions. They only translate: the file URLs of the dragging pasteboard
//! (`readObjectsForClasses:options:` with `NSURL` and `NSPasteboardURLReadingFileURLsOnlyKey`) and the pointer in the
//! frame's DIPs go to the shell, which asks the UI state what the drop would do. A drag that offers no file URL (a
//! promised file of Mail or Photos, text) is `NotFiles` and the state refuses it with the reason.
//!
//! Developer.apple.com: `NSDraggingDestination`, `NSDraggingInfo.draggingLocation` (window coordinates),
//! `NSView.registerForDraggedTypes(_:)`, `NSPasteboard.readObjects(forClasses:options:)`,
//! `NSPasteboard.ReadingOptionKey.urlReadingFileURLsOnly`, `NSFilePromiseReceiver.readableDraggedTypes`.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, ProtocolObject};
use objc2::{ClassType, msg_send};
use objc2_app_kit::{
    NSDragOperation, NSDraggingInfo, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardURLReadingFileURLsOnlyKey,
    NSView,
};
use objc2_foundation::{NSArray, NSCopying, NSDictionary, NSNumber, NSString, NSURL};
use swapdesk_ui::{DropEffect, SystemDragData};

/// The types the views register for: file URLs, and what a promised file offers (so a promise reaches the state and
/// is refused with its reason instead of the drag passing over the window as if it were not there).
fn dragged_types() -> Retained<NSArray<NSString>> {
    // SAFETY: a constant of AppKit.
    let mut types: Vec<Retained<NSString>> = vec![unsafe { NSPasteboardTypeFileURL }.copy()];
    if let Some(receiver) = AnyClass::get(c"NSFilePromiseReceiver") {
        // SAFETY: a class method of AppKit that returns an array of pasteboard types.
        let promised: Option<Retained<NSArray<NSString>>> = unsafe { msg_send![receiver, readableDraggedTypes] };
        if let Some(promised) = promised {
            types.extend(promised.iter());
        }
    }
    NSArray::from_retained_slice(&types)
}

/// Makes `view` a destination for files.
pub(crate) fn register(view: &NSView) {
    view.registerForDraggedTypes(&dragged_types());
}

/// What the dragging pasteboard carries: the file URLs (no file-system call: the worker says later which are folders), or "not files".
fn read(pasteboard: &NSPasteboard) -> SystemDragData {
    let classes = NSArray::from_slice(&[NSURL::class()]);
    // SAFETY: a constant of AppKit.
    let key = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
    let yes = NSNumber::new_bool(true);
    let options = NSDictionary::from_slices(&[key], &[yes.as_ref()]);
    // SAFETY: the classes are NSURL and the option is the boolean the key takes.
    let objects = unsafe { pasteboard.readObjectsForClasses_options(&classes, Some(&options)) };
    let items: Vec<PathBuf> = objects
        .map(|array| {
            array
                .iter()
                .filter_map(|o| o.downcast::<NSURL>().ok().and_then(|url| url.path()))
                .map(|p| PathBuf::from(p.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() { SystemDragData::NotFiles } else { SystemDragData::Files(items) }
}

/// The pointer in the frame's DIPs: the views are flipped children of one flipped container, and the canvas covers it.
fn point(view: &NSView, info: &ProtocolObject<dyn NSDraggingInfo>) -> (f32, f32) {
    let location = info.draggingLocation();
    // SAFETY: a plain getter of the view hierarchy, on the main thread.
    let parent = unsafe { view.superview() };
    let p = match parent {
        Some(parent) => parent.convertPoint_fromView(location, None),
        None => view.convertPoint_fromView(location, None),
    };
    (p.x as f32, p.y as f32)
}

fn operation(effect: DropEffect) -> NSDragOperation {
    match effect {
        DropEffect::Copy => NSDragOperation::Copy,
        DropEffect::None => NSDragOperation::None,
    }
}

/// `draggingEntered:`.
pub(crate) fn entered(view: &NSView, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    let data = read(&info.draggingPasteboard());
    operation(crate::shell::system_drag_enter(data, point(view, info)))
}

/// `draggingUpdated:` (called often, also while the pointer rests; the paths were read at `draggingEntered:`).
pub(crate) fn updated(view: &NSView, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    operation(crate::shell::system_drag_over(point(view, info)))
}

/// `draggingExited:`.
pub(crate) fn exited() {
    crate::shell::system_drag_leave();
}

/// `performDragOperation:`: true when the drop was accepted.
pub(crate) fn perform(view: &NSView, info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    crate::shell::system_drop(point(view, info)) == DropEffect::Copy
}
