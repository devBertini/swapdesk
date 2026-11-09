//! `ClipboardProvider` (contract C3) on `NSPasteboard.generalPasteboard` (F-CLIP-01, PROTOCOL.md 7.5): UTF-8 text
//! through `NSPasteboardTypeString` and PNG through `NSPasteboardTypePNG`, with TIFF (screenshots, "copy image")
//! converted to PNG on read through `NSBitmapImageRep`. Content marked `org.nspasteboard.ConcealedType` by a password
//! manager reads as `Concealed` and is never returned. The decisions are in [`crate::clip_logic`].
//!
//! Change detection is `NSPasteboard.changeCount`, which the contract polls.
//!
//! Threads: Apple's Thread Safety Summary (developer.apple.com/library/archive/documentation/Cocoa/Conceptual/
//! Multithreading/ThreadSafetySummary/ThreadSafetySummary.html, read 2026-11-08) lists no `NSPasteboard` restriction:
//! the AppKit classes it names as main thread only are `NSCell` and `NSView`, and as thread-unsafe `NSGraphicsContext`,
//! `NSImage`, `NSResponder` and `NSWindow`. So the provider works from any one thread at a time (`&mut self`), with an
//! autorelease pool around each call because the helper's threads run no run loop. `NSBitmapImageRep` is not in either
//! list and is used only inside one call.
//!
//! Limits: like the Windows provider this refuses to copy out more than 64 MiB (reported as `Unsupported`) and
//! refuses to write an item over the 7.5 limits (`InvalidInput`); the sync engine checks the limits of what is read.

use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
};
use objc2_foundation::{NSData, NSDictionary, NSString};
use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider, ErrorKind, PlatformError, Result};
use swapdesk_proto::{MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT};

use crate::clip_logic::{Pick, TYPE_TIFF, pick, png_exact_len};

/// The most this module copies out of the clipboard for one format.
const COPY_CAP: usize = 64 * 1024 * 1024;

fn err(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.to_owned() }
}

#[derive(Default)]
pub struct MacClipboard;

impl MacClipboard {
    pub fn new() -> Self {
        Self
    }
}

fn type_string() -> &'static NSString {
    // SAFETY: `NSPasteboardTypeString` is an immutable constant string exported by AppKit.
    unsafe { NSPasteboardTypeString }
}

fn type_png() -> &'static NSString {
    // SAFETY: `NSPasteboardTypePNG` is an immutable constant string exported by AppKit.
    unsafe { NSPasteboardTypePNG }
}

fn type_names(pb: &NSPasteboard) -> Vec<String> {
    let Some(types) = pb.types() else { return Vec::new() };
    (0..types.count()).map(|i| types.objectAtIndex(i).to_string()).collect()
}

fn png_of_tiff(tiff: &NSData) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(tiff)?;
    let props: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    // SAFETY: `props` is an empty dictionary of the declared key and value types; PNG takes no properties.
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(png.to_vec())
}

impl ClipboardProvider for MacClipboard {
    fn change_count(&mut self) -> Result<u64> {
        autoreleasepool(|_| {
            let n = NSPasteboard::generalPasteboard().changeCount();
            Ok(u64::try_from(n).unwrap_or(0))
        })
    }

    fn read(&mut self) -> Result<ClipRead> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let read = match pick(&type_names(&pb)) {
                Pick::Empty => ClipRead::Empty,
                Pick::Concealed => ClipRead::Concealed,
                Pick::Unsupported => ClipRead::Unsupported,
                Pick::Text => match pb.stringForType(type_string()) {
                    Some(s) if s.len() <= COPY_CAP => ClipRead::Content(ClipContent::Text(s.to_string())),
                    _ => ClipRead::Unsupported,
                },
                Pick::Png => match pb.dataForType(type_png()) {
                    Some(d) if d.length() <= COPY_CAP => {
                        let bytes = d.to_vec();
                        png_exact_len(&bytes)
                            .map_or(ClipRead::Unsupported, |n| ClipRead::Content(ClipContent::Png(bytes[..n].to_vec())))
                    }
                    _ => ClipRead::Unsupported,
                },
                Pick::TiffAsPng => {
                    let tiff = pb.dataForType(&NSString::from_str(TYPE_TIFF));
                    match tiff {
                        Some(d) if d.length() <= COPY_CAP => {
                            png_of_tiff(&d).map_or(ClipRead::Unsupported, |p| ClipRead::Content(ClipContent::Png(p)))
                        }
                        _ => ClipRead::Unsupported,
                    }
                }
            };
            Ok(read)
        })
    }

    fn write(&mut self, content: &ClipContent) -> Result<()> {
        match content {
            ClipContent::Text(t) if t.len() <= MAX_CLIPBOARD_TEXT as usize => autoreleasepool(|_| {
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                if pb.setString_forType(&NSString::from_str(t), type_string()) {
                    Ok(())
                } else {
                    Err(err(ErrorKind::Os, "clipboard write", "setString:forType: refused"))
                }
            }),
            ClipContent::Png(p) if p.len() <= MAX_CLIPBOARD_PNG as usize && png_exact_len(p) == Some(p.len()) => {
                autoreleasepool(|_| {
                    let pb = NSPasteboard::generalPasteboard();
                    pb.clearContents();
                    if pb.setData_forType(Some(&NSData::with_bytes(p)), type_png()) {
                        Ok(())
                    } else {
                        Err(err(ErrorKind::Os, "clipboard write", "setData:forType: refused"))
                    }
                })
            }
            _ => Err(err(ErrorKind::InvalidInput, "clipboard write", "over the limit or not a PNG")),
        }
    }
}
