//! The CoreFoundation, CoreGraphics, CoreText and ImageIO functions the shared-screen painter uses, declared by hand
//! as plain C functions over opaque references (D37: no new crate). Signatures from Apple's headers as the objc2
//! bindings (objc2-core-graphics 0.3.2, objc2-core-text 0.3.2) transcribe them:
//! https://developer.apple.com/documentation/coregraphics, https://developer.apple.com/documentation/coretext,
//! https://developer.apple.com/documentation/imageio.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFURLRef = *const c_void;
pub type CFMutableAttributedStringRef = *mut c_void;
pub type CGContextRef = *mut c_void;
pub type CGColorSpaceRef = *mut c_void;
pub type CGColorRef = *mut c_void;
pub type CGPathRef = *const c_void;
pub type CGImageRef = *mut c_void;
pub type CTFontDescriptorRef = *const c_void;
pub type CTFontRef = *const c_void;
pub type CTTypesetterRef = *const c_void;
pub type CTLineRef = *const c_void;
pub type CGImageDestinationRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

impl CGRect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CGAffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CFRange {
    pub location: isize,
    pub length: isize,
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: isize = 3;
pub const kCFNumberFloat64Type: isize = 6;
/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`: BGRA in memory, premultiplied (as WIC's PBGRA).
pub const BITMAP_BGRA_PREMULTIPLIED: u32 = 2 | (2 << 12);
pub const kCGLineCapButt: i32 = 0;
pub const kCGLineCapRound: i32 = 1;
pub const kCGLineJoinMiter: i32 = 0;
pub const kCGLineJoinRound: i32 = 1;
pub const kCTLineTruncationEnd: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFBooleanTrue: CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, length: isize) -> CFDataRef;
    pub fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> CFStringRef;
    pub fn CFStringGetLength(s: CFStringRef) -> isize;
    pub fn CFNumberCreate(alloc: CFTypeRef, the_type: isize, value_ptr: *const c_void) -> CFNumberRef;
    pub fn CFAttributedStringCreateMutable(alloc: CFTypeRef, max_length: isize) -> CFMutableAttributedStringRef;
    pub fn CFAttributedStringReplaceString(s: CFMutableAttributedStringRef, range: CFRange, replacement: CFStringRef);
    pub fn CFAttributedStringSetAttribute(
        s: CFMutableAttributedStringRef,
        range: CFRange,
        name: CFStringRef,
        value: CFTypeRef,
    );
    pub fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFTypeRef,
        buffer: *const u8,
        len: isize,
        is_directory: u8,
    ) -> CFURLRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub static kCGColorSpaceSRGB: CFStringRef;
    pub fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    pub fn CGColorCreateSRGB(red: f64, green: f64, blue: f64, alpha: f64) -> CGColorRef;
    pub fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    pub fn CGBitmapContextGetData(c: CGContextRef) -> *mut c_void;
    pub fn CGBitmapContextGetBytesPerRow(c: CGContextRef) -> usize;
    pub fn CGBitmapContextCreateImage(c: CGContextRef) -> CGImageRef;
    pub fn CGContextSaveGState(c: CGContextRef);
    pub fn CGContextRestoreGState(c: CGContextRef);
    pub fn CGContextTranslateCTM(c: CGContextRef, tx: f64, ty: f64);
    pub fn CGContextScaleCTM(c: CGContextRef, sx: f64, sy: f64);
    pub fn CGContextSetFillColorWithColor(c: CGContextRef, color: CGColorRef);
    pub fn CGContextSetStrokeColorWithColor(c: CGContextRef, color: CGColorRef);
    pub fn CGContextSetLineWidth(c: CGContextRef, width: f64);
    pub fn CGContextSetLineCap(c: CGContextRef, cap: i32);
    pub fn CGContextSetLineJoin(c: CGContextRef, join: i32);
    pub fn CGContextSetLineDash(c: CGContextRef, phase: f64, lengths: *const f64, count: usize);
    pub fn CGContextBeginPath(c: CGContextRef);
    pub fn CGContextMoveToPoint(c: CGContextRef, x: f64, y: f64);
    pub fn CGContextAddLineToPoint(c: CGContextRef, x: f64, y: f64);
    pub fn CGContextAddCurveToPoint(c: CGContextRef, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64);
    pub fn CGContextClosePath(c: CGContextRef);
    pub fn CGContextAddPath(c: CGContextRef, path: CGPathRef);
    pub fn CGContextFillPath(c: CGContextRef);
    pub fn CGContextStrokePath(c: CGContextRef);
    pub fn CGContextFillRect(c: CGContextRef, rect: CGRect);
    pub fn CGContextClearRect(c: CGContextRef, rect: CGRect);
    pub fn CGContextFillEllipseInRect(c: CGContextRef, rect: CGRect);
    pub fn CGContextStrokeEllipseInRect(c: CGContextRef, rect: CGRect);
    pub fn CGContextClipToRect(c: CGContextRef, rect: CGRect);
    pub fn CGContextSetTextMatrix(c: CGContextRef, t: CGAffineTransform);
    pub fn CGContextSetTextPosition(c: CGContextRef, x: f64, y: f64);
    pub fn CGContextSetShouldSmoothFonts(c: CGContextRef, should: bool);
    pub fn CGPathCreateWithRoundedRect(
        rect: CGRect,
        corner_width: f64,
        corner_height: f64,
        transform: *const CGAffineTransform,
    ) -> CGPathRef;
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    pub static kCTFontAttributeName: CFStringRef;
    pub static kCTKernAttributeName: CFStringRef;
    pub static kCTForegroundColorFromContextAttributeName: CFStringRef;
    pub fn CTFontManagerCreateFontDescriptorFromData(data: CFDataRef) -> CTFontDescriptorRef;
    pub fn CTFontDescriptorCreateCopyWithVariation(
        original: CTFontDescriptorRef,
        variation_identifier: CFNumberRef,
        variation_value: f64,
    ) -> CTFontDescriptorRef;
    pub fn CTFontCreateWithFontDescriptor(
        descriptor: CTFontDescriptorRef,
        size: f64,
        matrix: *const CGAffineTransform,
    ) -> CTFontRef;
    pub fn CTTypesetterCreateWithAttributedString(string: CFMutableAttributedStringRef) -> CTTypesetterRef;
    pub fn CTTypesetterSuggestLineBreak(typesetter: CTTypesetterRef, start_index: isize, width: f64) -> isize;
    pub fn CTTypesetterCreateLine(typesetter: CTTypesetterRef, string_range: CFRange) -> CTLineRef;
    pub fn CTLineCreateWithAttributedString(string: CFMutableAttributedStringRef) -> CTLineRef;
    pub fn CTLineGetTypographicBounds(line: CTLineRef, ascent: *mut f64, descent: *mut f64, leading: *mut f64) -> f64;
    pub fn CTLineGetTrailingWhitespaceWidth(line: CTLineRef) -> f64;
    pub fn CTLineCreateTruncatedLine(line: CTLineRef, width: f64, truncation_type: u32, token: CTLineRef) -> CTLineRef;
    pub fn CTLineDraw(line: CTLineRef, context: CGContextRef);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    pub fn CGImageDestinationCreateWithURL(
        url: CFURLRef,
        type_: CFStringRef,
        count: usize,
        options: CFTypeRef,
    ) -> CGImageDestinationRef;
    pub fn CGImageDestinationAddImage(dest: CGImageDestinationRef, image: CGImageRef, properties: CFTypeRef);
    pub fn CGImageDestinationFinalize(dest: CGImageDestinationRef) -> bool;
}

/// A Core Foundation reference this code created (a Create or Copy call), released on drop.
pub struct Owned(pub CFTypeRef);

impl Owned {
    /// Takes a reference returned by a Create or Copy call; `None` for a null one.
    pub fn new(r: *const c_void) -> Option<Owned> {
        (!r.is_null()).then_some(Owned(r))
    }

    pub fn get(&self) -> CFTypeRef {
        self.0
    }

    pub fn mut_ptr(&self) -> *mut c_void {
        self.0.cast_mut()
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the reference came from a Create or Copy call and is released once.
        unsafe { CFRelease(self.0) };
    }
}

/// A CFString of `s`.
pub fn cf_string(s: &str) -> Option<Owned> {
    // SAFETY: the bytes outlive the call, which copies them.
    Owned::new(unsafe {
        CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, kCFStringEncodingUTF8, 0)
    })
}
